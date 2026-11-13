//! Process lifecycle: launch, attach, detach, resume and wait.

use std::time::Duration;

use super::{ProcessOwnership, SIGKILL, Session, SessionState};
use crate::arch;
use crate::backend::{LaunchSpec, ResumeRequest, StopEvent};
use crate::error::{DebuggerError, Result};
use crate::symbols::SymbolCache;
use crate::types::ProcessId;

impl Session
{
    /// Typed error for `launch` and `attach` while a process exists.
    fn require_no_process(&self) -> Result<()>
    {
        match self.state {
            SessionState::NotStarted | SessionState::Exited => Ok(()),
            SessionState::Stopped | SessionState::Running => Err(DebuggerError::AlreadyAttached),
        }
    }

    /// Enter the initial stop of a new process.
    fn begin_process(&mut self, pid: ProcessId)
    {
        self.clear_process();
        // Images of an earlier process may sit at other addresses: start a fresh cache.
        self.symbols = SymbolCache::new();
        self.loaded_images.clear();
        self.pid = Some(pid);
        self.state = SessionState::Stopped;
        self.ownership = None;
        self.last_stop = None;
        self.sync_threads(None);
    }

    /// Start `spec.program`, stopped before its first instruction.
    ///
    /// Afterwards the session is [`SessionState::Stopped`], the first thread is
    /// selected and [`last_stop`](Self::last_stop) is `None`.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::AlreadyAttached`] while a process exists, or the backend's
    /// error (a [`BackendError::PermissionDenied`](crate::backend::BackendError::PermissionDenied)
    /// surfaces as [`DebuggerError::PermissionDenied`]).
    pub fn launch(&mut self, spec: &LaunchSpec) -> Result<ProcessId>
    {
        self.require_no_process()?;
        let pid = self.backend.launch(spec)?;
        self.begin_process(pid);
        self.ownership = Some(ProcessOwnership::Launched);
        Ok(pid)
    }

    /// Attach to the running process `pid` and stop it.
    ///
    /// Afterwards the session is [`SessionState::Stopped`], the first thread is
    /// selected and [`last_stop`](Self::last_stop) is `None`.
    ///
    /// # Errors
    ///
    /// As for [`launch`](Self::launch).
    pub fn attach(&mut self, pid: ProcessId) -> Result<()>
    {
        self.require_no_process()?;
        self.backend.attach(pid)?;
        self.begin_process(pid);
        self.ownership = Some(ProcessOwnership::Attached);
        Ok(())
    }

    /// Remove every inserted trap, then release the process, which keeps running.
    ///
    /// Each inserted software breakpoint gets its original bytes back first; the
    /// backend knows nothing about them. Afterwards the session is
    /// [`SessionState::NotStarted`] and the breakpoint table is empty.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error. If restoring a trap fails, the process is
    /// not detached: breakpoints already restored are left disabled, the others stay
    /// inserted, and the session stays stopped. If the backend's `detach` fails, every
    /// breakpoint is left disabled and the session stays stopped.
    pub fn detach(&mut self) -> Result<()>
    {
        self.require_stopped()?;
        self.restore_all_traps()?;
        self.backend.detach()?;
        self.clear_process();
        self.state = SessionState::NotStarted;
        self.ownership = None;
        self.last_stop = None;
        Ok(())
    }

    /// Continue every thread; the session becomes [`SessionState::Running`].
    ///
    /// As the backend contract requires, events still queued from the last stop (for
    /// example two threads that stopped at once) are drained first with a zero-timeout
    /// wait. If one is pending, the process is not resumed: the session still becomes
    /// running and the next [`wait`](Self::wait) returns that event.
    ///
    /// A signal held by the backend after a [`StopEvent::Signal`] stop is discarded: the
    /// program never sees it. Use [`resume_with_signal`](Self::resume_with_signal) to pass
    /// it on.
    ///
    /// Known gap (T07, issue #33): resuming from a breakpoint stop does not step over the
    /// trap first.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error (the session then stays stopped).
    pub fn resume(&mut self) -> Result<()>
    {
        self.resume_request(ResumeRequest::continue_all())
    }

    /// Like [`resume`](Self::resume), but when the last stop was
    /// [`StopEvent::Signal`] that signal is passed on to its thread
    /// (`ResumeRequest::continue_all().with_signal(thread, signal)`), so the program
    /// receives it. After any other stop it is the same as `resume`.
    ///
    /// # Errors
    ///
    /// As for [`resume`](Self::resume).
    pub fn resume_with_signal(&mut self) -> Result<()>
    {
        let mut request = ResumeRequest::continue_all();
        if let Some(StopEvent::Signal { thread, signal }) = self.last_stop.as_ref() {
            request = request.with_signal(*thread, *signal);
        }
        self.resume_request(request)
    }

    fn resume_request(&mut self, request: ResumeRequest) -> Result<()>
    {
        self.require_stopped()?;
        if let Some(event) = self.backend.wait(Some(Duration::ZERO))? {
            self.deferred = Some(event);
        } else {
            self.backend.resume(&request)?;
        }
        self.state = SessionState::Running;
        self.last_stop = None;
        Ok(())
    }

    /// Ask a running process to stop; returns immediately.
    ///
    /// The stop is reported by a later [`wait`](Self::wait), as
    /// [`StopEvent::Interrupted`] or as another event that raced it. While the process is
    /// stopped this does nothing and calls no backend method. The session state does not
    /// change until `wait` returns the event.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, or the backend's error (the
    /// state does not change).
    pub fn interrupt(&mut self) -> Result<()>
    {
        match self.state {
            SessionState::Stopped => Ok(()),
            SessionState::Running => self.backend.interrupt().map_err(Into::into),
            SessionState::NotStarted | SessionState::Exited => Err(DebuggerError::NotAttached),
        }
    }

    /// Terminate the process, stopped or running, and return the event that records it.
    ///
    /// The backend reports no terminal event for a kill, so the session makes one:
    /// afterwards it is [`SessionState::Exited`], [`last_stop`](Self::last_stop) is
    /// `StopEvent::Killed { signal: SIGKILL }` (also the return value), and the pid,
    /// threads, selection and breakpoint table are cleared. Traps are not restored: the
    /// memory that held them is gone.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, or the backend's error (the
    /// state and the breakpoint table do not change).
    pub fn kill(&mut self) -> Result<StopEvent>
    {
        self.require_process()?;
        self.backend.kill()?;
        self.clear_process();
        self.state = SessionState::Exited;
        self.ownership = None;
        let event = StopEvent::Killed { signal: SIGKILL };
        self.last_stop = Some(event.clone());
        Ok(event)
    }

    /// Wait for the process to stop and return the event.
    ///
    /// - While running: `timeout = Some(d)` returns `Ok(None)` if nothing happened within
    ///   `d` (the session stays running); `None` blocks until an event arrives.
    /// - While stopped: returns an event still queued from the last stop, if any. With a
    ///   finite timeout `Ok(None)` means none is pending. With `timeout = None` the
    ///   backend is polled without blocking, and [`DebuggerError::NotRunning`] is returned
    ///   when nothing is pending, because the wait would never end.
    ///
    /// On an event the session becomes [`SessionState::Stopped`], or
    /// [`SessionState::Exited`] for a terminal event. A thread named by the event is
    /// selected. For [`StopEvent::Breakpoint`] the pc is mapped with
    /// [`Arch::pc_after_trap`](crate::arch::Arch::pc_after_trap), the returned event
    /// carries the mapped pc, and a hit is recorded on an enabled breakpoint at that
    /// address.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotRunning`] as
    /// described above, or the backend's error (the state does not change).
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<StopEvent>>
    {
        self.require_process()?;
        let event = match self.state {
            SessionState::Running => match self.deferred.take() {
                Some(event) => Some(event),
                None => self.backend.wait(timeout)?,
            },
            _ => match timeout {
                Some(_) => self.backend.wait(timeout)?,
                None => Some(self.backend.wait(Some(Duration::ZERO))?.ok_or(DebuggerError::NotRunning)?),
            },
        };
        Ok(event.map(|event| self.handle_stop(event)))
    }

    /// Apply a stop event to the session and return it as the caller sees it.
    fn handle_stop(&mut self, event: StopEvent) -> StopEvent
    {
        if event.is_terminal() {
            self.clear_process();
            self.state = SessionState::Exited;
            self.last_stop = Some(event.clone());
            return event;
        }

        let event = match event {
            StopEvent::Breakpoint { thread, pc } => {
                let pc = arch::for_architecture(self.backend.architecture()).map_or(pc, |arch| arch.pc_after_trap(pc));
                self.breakpoints.record_hit(pc);
                StopEvent::Breakpoint { thread, pc }
            }
            other => other,
        };
        self.state = SessionState::Stopped;
        self.sync_threads(event.thread());
        self.last_stop = Some(event.clone());
        event
    }
}
