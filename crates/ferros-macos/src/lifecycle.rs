//! Process death notification and reaping for the Mach backend.
//!
//! Each traced pid has one kqueue watcher (`EVFILT_PROC`, `NOTE_EXIT | NOTE_EXITSTATUS`).
//! The watcher alone calls `waitpid`: launch children are reaped and attached targets
//! are returned to their original parent by Darwin's tracer wait semantics. Only terminal
//! statuses become events; ptrace's intermediate signal stops are handled by the Mach
//! loop. A traced child whose original parent is this process needs both a tracer wait
//! and a parent wait; both finish before its terminal event is published. Death is marked under the exception-state lock before the event is published,
//! so dead-thread reply failures cannot obscure it. Backend teardown cancels and joins
//! the watcher before releasing backend resources.
//!
//! Kqueue waits have a 20 ms deadline; reaping polls with WNOHANG so cancellation and
//! kill cleanup remain bounded even when a target is suspended. No SIGCHLD handler is
//! installed, and no wait for another backend's pid is performed.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ferros_core::backend::{BackendError, BackendResult, StopEvent};
use libc::{mach_port_t, pid_t};

use crate::exception::{ExceptionSharedState, LoopEvent, lock_shared};

/// Test whether the task send right has become a dead name.
pub(crate) fn task_is_dead(task: mach_port_t) -> bool
{
    let mut kind = 0;
    // SAFETY: `kind` is writable and `task` belongs to this process's IPC space.
    let kr = unsafe { crate::ffi::mach_port_type(mach2::traps::mach_task_self(), task, &mut kind) };
    kr != mach2::kern_return::KERN_SUCCESS || kind & (1 << (mach2::port::MACH_PORT_RIGHT_DEAD_NAME + 16)) != 0
}

/// Whether `pid` is stopped by a signal (BSD state `SSTOP`, `ps` state `T`).
pub(crate) fn is_job_control_stopped(pid: pid_t) -> bool
{
    // SAFETY: zero is a valid bit pattern for this plain C struct.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is writable for `size` bytes and outlives the call.
    let written = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    written == size && info.pbi_status == libc::SSTOP
}

/// A cancellable exit watcher that owns the tracee's waitpid calls.
pub(crate) struct ExitWatcher
{
    cancel: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    finished: mpsc::Receiver<BackendResult<()>>,
}

impl ExitWatcher
{
    pub(crate) fn start(
        pid: pid_t,
        shared: Arc<Mutex<ExceptionSharedState>>,
        events: mpsc::Sender<LoopEvent>,
    ) -> BackendResult<Self>
    {
        // SAFETY: kqueue takes no arguments and returns a new owned descriptor.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(os_error("kqueue"));
        }
        // SAFETY: `fd` was just created and ownership is transferred exactly once.
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        let change = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: `change` is one initialized event; no output is requested.
        if unsafe { libc::kevent(fd, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(os_error("register process exit"));
        }
        let (cancel, cancellation) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("ferros-exit".into())
            .spawn(move || {
                let result = watch(queue, pid, &shared, &events, &cancellation);
                if let Err(err) = &result {
                    let _ = events.send(Err(err.clone()));
                }
                let _ = done.send(result);
            })
            .map_err(|err| BackendError::Os {
                code: i64::from(err.raw_os_error().unwrap_or(-1)),
                message: format!("start exit watcher: {err}"),
            })?;
        Ok(Self {
            cancel,
            thread: Some(thread),
            finished,
        })
    }

    /// Wait for reaping after kill, bounded by the caller's deadline.
    pub(crate) fn wait(&self, timeout: Duration) -> BackendResult<()>
    {
        self.finished.recv_timeout(timeout).map_err(|_| BackendError::Os {
            code: -1,
            message: "timed out waiting for process death and reaping".into(),
        })?
    }
}

impl Drop for ExitWatcher
{
    fn drop(&mut self)
    {
        let _ = self.cancel.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch(
    queue: OwnedFd,
    pid: pid_t,
    shared: &Mutex<ExceptionSharedState>,
    events: &mpsc::Sender<LoopEvent>,
    cancel: &mpsc::Receiver<()>,
) -> BackendResult<()>
{
    loop {
        if cancel.try_recv() != Err(mpsc::TryRecvError::Empty) {
            return Ok(());
        }
        // SAFETY: zero is valid for the scalar fields and pointer of kevent.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        // SAFETY: one writable event and a valid timeout outlive the call.
        let count = unsafe { libc::kevent(queue.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, &timeout) };
        if count < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(os_error("wait for process exit"));
        }
        if count == 0 {
            continue;
        }
        if event.flags & libc::EV_ERROR != 0 {
            return Err(BackendError::Os {
                code: event.data as i64,
                message: "process exit notification failed".into(),
            });
        }
        // NOTE_EXIT can precede the status becoming waitable. No other path reaps
        // while this watcher exists, so retry until terminal status or cancellation.
        lock_shared(shared).dead = true;
        let mut terminal = None;
        loop {
            let mut status = 0;
            // SAFETY: pid is our child/tracee; status is writable. WNOHANG never blocks.
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == pid && (libc::WIFEXITED(status) || libc::WIFSIGNALED(status)) {
                let stop = if libc::WIFEXITED(status) {
                    StopEvent::Exited {
                        code: libc::WEXITSTATUS(status),
                    }
                } else {
                    StopEvent::Killed {
                        signal: libc::WTERMSIG(status),
                    }
                };
                terminal = Some(stop);
                // Darwin first hands a traced zombie back to its original parent,
                // even when that parent is us. A second wait reaps our launch child.
                continue;
            }
            if result < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
                && let Some(stop) = terminal.take()
            {
                let _state = lock_shared(shared);
                let _ = events.send(Ok(stop));
                return Ok(());
            }
            if result < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return Err(os_error("reap traced process"));
            }
            if cancel.recv_timeout(Duration::from_millis(5)) != Err(mpsc::RecvTimeoutError::Timeout) {
                return Ok(());
            }
        }
    }
}

fn os_error(operation: &str) -> BackendError
{
    let err = std::io::Error::last_os_error();
    BackendError::Os {
        code: i64::from(err.raw_os_error().unwrap_or(-1)),
        message: format!("{operation}: {err}"),
    }
}
