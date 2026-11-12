//! # macOS Task Management
//!
//! [`MacOSBackend`]: the [`Backend`] implementation for macOS, built on Mach tasks,
//! threads and exception ports.
//!
//! On macOS, a **task** represents a process, and a **thread** represents
//! a thread within that process. To debug a process, the backend:
//!
//! 1. Gets a Mach port to the task (using `task_for_pid()`)
//! 2. Enumerates threads in the task (using `task_threads()`)
//! 3. Reads/writes thread state (using `thread_get_state()` / `thread_set_state()`)
//! 4. Receives exceptions (breakpoints, faults) on an exception port
//!
//! ## Mach Ports
//!
//! Mach ports are communication channels to kernel objects (tasks, threads, etc.).
//! They're like file descriptors, but for kernel objects. When you get a task port,
//! you can use it to control that process.
//!
//! ## Suspend accounting
//!
//! *Stopped* means the backend holds at least one `task_suspend` on the task: the
//! suspended start of a launch, the `task_suspend` of an attach, one per exception stop
//! and one per interrupt. The count lives in the shared exception state; `resume`,
//! `detach` and `kill` balance it with `task_resume`.
//!
//! ## References
//!
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - [XNU Kernel Source](https://github.com/apple-oss-distributions/xnu) (for `task_for_pid` and `task_threads` implementation)

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ferros_core::backend::{
    Backend, BackendError, BackendResult, LaunchSpec, LoadedImage, ResumeRequest, StopEvent, ThreadAction,
};
use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, ThreadId};
use libc::{c_int, mach_msg_type_number_t, mach_port_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::exception_types::{
    EXC_MASK_ARITHMETIC, EXC_MASK_BAD_ACCESS, EXC_MASK_BAD_INSTRUCTION, EXC_MASK_BREAKPOINT, EXC_MASK_SOFTWARE,
    EXCEPTION_DEFAULT, MACH_EXCEPTION_CODES, exception_behavior_t, exception_mask_t,
};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::mach_port::{mach_port_allocate, mach_port_destroy, mach_port_insert_right};
#[cfg(target_os = "macos")]
use mach2::message::MACH_MSG_TYPE_MAKE_SEND;
#[cfg(target_os = "macos")]
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE};
#[cfg(target_os = "macos")]
use mach2::task::{task_resume, task_set_exception_ports, task_suspend, task_threads};
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;

use crate::error::MachError;
use crate::exception::{self, ExceptionLoopCommand, ExceptionSharedState, lock_shared};
use crate::memory::{MemoryCache, MemoryProtectionGuard, get_memory_regions, write_memory};
#[cfg(target_arch = "aarch64")]
use crate::registers::{read_registers_arm64, write_registers_arm64};
#[cfg(target_arch = "x86_64")]
use crate::registers::{read_registers_x86_64, write_registers_x86_64};
use crate::{ffi, images, launch, threads};

/// How long `resume` waits for the exception loop to confirm its reply.
const EXCEPTION_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `kill` waits for a launched process to be reaped.
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// The three states of the [`Backend`] contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendState
{
    NoProcess,
    Stopped,
    Running,
}

/// Map an error of this crate's Mach code to the portable [`BackendError`].
///
/// This is the one place where `DebuggerError` (and, through it, [`MachError`]) becomes a
/// `BackendError`, at the [`Backend`] boundary:
///
/// - `ProcessNotFound`, and I/O errors of kind `NotFound` -> `NotFound`
/// - `PermissionDenied`, and I/O errors of kind `PermissionDenied` -> `PermissionDenied`
/// - `NotAttached`, `NotStopped`, `NotRunning`, `AlreadyAttached` -> `InvalidState`
/// - `InvalidArgument` -> `Unsupported` (unsupported architecture, bad argument strings)
/// - `Os { code, .. }` keeps its code; every other error -> `Os` with code -1 (or the
///   errno of an I/O error)
pub(crate) fn backend_error(err: DebuggerError) -> BackendError
{
    let message = err.to_string();
    match err {
        DebuggerError::Backend(err) => err,
        DebuggerError::ProcessNotFound(_) => BackendError::NotFound(message),
        DebuggerError::PermissionDenied(detail) => BackendError::PermissionDenied(detail),
        DebuggerError::NotAttached
        | DebuggerError::NotStopped
        | DebuggerError::NotRunning
        | DebuggerError::AlreadyAttached => BackendError::InvalidState(message),
        DebuggerError::InvalidArgument(detail) => BackendError::Unsupported(detail),
        DebuggerError::Os { code, message } => BackendError::Os { code, message },
        DebuggerError::Io(io) => match io.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound(io.to_string()),
            std::io::ErrorKind::PermissionDenied => BackendError::PermissionDenied(io.to_string()),
            _ => BackendError::Os {
                code: io.raw_os_error().map_or(-1, i64::from),
                message: io.to_string(),
            },
        },
        _ => BackendError::Os { code: -1, message },
    }
}

/// Map a failed `kern_return_t` to a [`BackendError`].
fn mach_error(kr: libc::kern_return_t, what: &str) -> BackendError
{
    match backend_error(DebuggerError::from(MachError::from(kr))) {
        BackendError::Os { code, message } => BackendError::Os {
            code,
            message: format!("{what}: {message}"),
        },
        other => other,
    }
}

/// Read the registers of `thread` for `architecture`.
pub(crate) fn read_registers_for_port(architecture: Architecture, thread: thread_act_t) -> Result<Registers>
{
    match architecture {
        Architecture::Arm64 => {
            #[cfg(target_arch = "aarch64")]
            {
                read_registers_arm64(thread)
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                let _ = thread;
                Err(DebuggerError::InvalidArgument(
                    "arm64 register access not supported on this build".to_string(),
                ))
            }
        }
        Architecture::X86_64 => {
            #[cfg(target_arch = "x86_64")]
            {
                read_registers_x86_64(thread)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                let _ = thread;
                Err(DebuggerError::InvalidArgument(
                    "x86_64 register access not supported on this build".to_string(),
                ))
            }
        }
        Architecture::Unknown(label) => Err(DebuggerError::InvalidArgument(format!("Unsupported architecture: {label}"))),
    }
}

/// Write `regs` to `thread` for `architecture`.
fn write_registers_to_port(architecture: Architecture, thread: thread_act_t, regs: &Registers) -> Result<()>
{
    match architecture {
        Architecture::Arm64 => {
            #[cfg(target_arch = "aarch64")]
            {
                write_registers_arm64(thread, regs)
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                let _ = (thread, regs);
                Err(DebuggerError::InvalidArgument(
                    "arm64 register access not supported on this build".to_string(),
                ))
            }
        }
        Architecture::X86_64 => {
            #[cfg(target_arch = "x86_64")]
            {
                write_registers_x86_64(thread, regs)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                let _ = (thread, regs);
                Err(DebuggerError::InvalidArgument(
                    "x86_64 register access not supported on this build".to_string(),
                ))
            }
        }
        Architecture::Unknown(label) => Err(DebuggerError::InvalidArgument(format!("Unsupported architecture: {label}"))),
    }
}

/// macOS backend using Mach APIs.
///
/// Implements [`Backend`]; drive it through [`ferros_core::Session`]. It holds the task
/// and thread ports of one target process, the Mach exception handler thread, and the
/// channel the handler reports stops on.
///
/// ## Lifecycle
///
/// 1. Create: `MacOSBackend::new()`
/// 2. `launch(spec)` or `attach(pid)`: the target is *Stopped*
/// 3. `resume`, `wait`, registers, memory, ...
/// 4. `detach()` (the target keeps running) or `kill()`
///
/// Dropping a backend that still has a process kills it if the backend launched it,
/// and detaches from it otherwise.
///
/// ## Thread Safety
///
/// [`Send`] but not [`Sync`]: use it from one thread at a time.
pub struct MacOSBackend
{
    /// Mach port to the target process (task), from `task_for_pid()`. 0 means none.
    ///
    /// See: [mach_port_t documentation](https://developer.apple.com/documentation/kernel/mach_port_t)
    task: mach_port_t,
    /// Thread ports of the target task, as of the last enumeration.
    threads: Vec<thread_act_t>,
    /// Process ID of the target process.
    pid: ProcessId,
    /// Architecture metadata.
    architecture: Architecture,
    /// Contract state.
    state: BackendState,
    /// Whether this backend spawned the target (and so reaps it on `kill`).
    launched: bool,
    /// Mach exception port used to receive stop notifications.
    exception_port: mach_port_t,
    /// Thread running the Mach exception handling loop.
    exception_thread: Option<thread::JoinHandle<()>>,
    /// Channel used to command the exception loop (continue/shutdown).
    exception_commands: Option<mpsc::Sender<ExceptionLoopCommand>>,
    /// Suspend count and held exception, shared with the exception loop.
    exception_state: Arc<Mutex<ExceptionSharedState>>,
    /// Sender side of the stop event channel (the exception loop holds a clone).
    events_tx: Option<mpsc::Sender<StopEvent>>,
    /// Receiver side of the stop event channel, read by `wait`.
    events_rx: Option<mpsc::Receiver<StopEvent>>,
    /// An event taken off the channel by `resume` while checking for pending events.
    peeked: Option<StopEvent>,
    /// Cached memory pages for repeated reads; cleared whenever the target may run.
    memory_cache: MemoryCache,
}

impl MacOSBackend
{
    /// Create a new macOS backend with no process.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Session;
    /// use ferros_core::types::ProcessId;
    /// use ferros_macos::MacOSBackend;
    ///
    /// let mut session = Session::new(Box::new(MacOSBackend::new()?));
    /// session.attach(ProcessId::from(12345))?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    ///
    /// ## Errors
    ///
    /// None at present; the `Result` leaves room for setup that can fail.
    pub fn new() -> Result<Self>
    {
        Ok(Self {
            task: 0,
            threads: Vec::new(),
            pid: ProcessId(0),
            architecture: Architecture::current(),
            state: BackendState::NoProcess,
            launched: false,
            exception_port: MACH_PORT_NULL,
            exception_thread: None,
            exception_commands: None,
            exception_state: Arc::new(Mutex::new(ExceptionSharedState::new())),
            events_tx: None,
            events_rx: None,
            peeked: None,
            memory_cache: MemoryCache::new(),
        })
    }

    /// Check if the debugger has permissions to attach to processes
    ///
    /// This function attempts to get a task port for the current process (self)
    /// to verify that debugging permissions are available.
    ///
    /// ## Returns
    ///
    /// - `Ok(true)`: Debugging permissions are available
    /// - `Ok(false)`: Debugging permissions are not available (need the entitlement)
    /// - `Err(...)`: Error occurred while checking permissions
    ///
    /// ## Note
    ///
    /// Even if this succeeds, attaching to other processes may still fail
    /// due to System Integrity Protection (SIP) restrictions.
    ///
    /// ## Errors
    ///
    /// Any `task_for_pid` failure other than `KERN_PROTECTION_FAILURE`.
    pub fn has_debugging_permissions(&self) -> Result<bool>
    {
        unsafe {
            let current_pid = libc::getpid();
            let mut task: mach_port_t = 0;
            let result = ffi::task_for_pid(mach_task_self(), current_pid, &mut task);

            match result {
                KERN_SUCCESS => Ok(true),
                libc::KERN_PROTECTION_FAILURE => Ok(false),
                _ => Err(DebuggerError::from(MachError::from(result))),
            }
        }
    }

    /// Mach thread ports of the target as of the last thread enumeration.
    ///
    /// Use them with the hardware breakpoint and watchpoint helpers in
    /// [`crate::breakpoints`], which the [`Backend`] interface does not cover.
    #[must_use]
    pub fn thread_ports(&self) -> &[thread_act_t]
    {
        &self.threads
    }

    /// Suspend a specific thread with `thread_suspend()` (unverified).
    ///
    /// The [`Backend`] interface does not expose per-thread suspension yet; this is kept
    /// for later use and bypasses the backend's state tracking.
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Thread ID is not valid (not in the thread list)
    /// - `SuspendFailed`: `thread_suspend()` failed
    pub fn suspend_thread(&mut self, thread_id: ThreadId) -> Result<()>
    {
        threads::ThreadManager::suspend_thread(&self.threads, thread_id)
    }

    /// Resume a specific thread with `thread_resume()` (unverified).
    ///
    /// See [`suspend_thread`](Self::suspend_thread).
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Thread ID is not valid (not in the thread list)
    /// - `ResumeFailed`: `thread_resume()` failed
    pub fn resume_thread(&mut self, thread_id: ThreadId) -> Result<()>
    {
        threads::ThreadManager::resume_thread(&self.threads, thread_id)
    }

    /// Obtain the Mach task port and thread list for `pid` and start the exception
    /// handler.
    ///
    /// Does not change the task's execution state; callers decide whether to suspend.
    /// On error nothing is kept.
    ///
    /// ## Mach APIs Used
    ///
    /// - **task_for_pid()**: Obtains a Mach task port for the process
    /// - **task_threads()**: Enumerates all threads in the task
    ///
    /// ## Permissions
    ///
    /// Requires root or the debugger entitlement to call `task_for_pid()`.
    ///
    /// See:
    /// - [task_for_pid(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_for_pid/)
    /// - [task_threads(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_threads/)
    fn attach_task(&mut self, pid: ProcessId) -> Result<()>
    {
        use tracing::{debug, info, trace};

        info!("Attaching to process {}", pid.0);
        debug!("Getting Mach task port for process {}", pid.0);

        unsafe {
            let mut task: mach_port_t = 0;
            trace!("Calling task_for_pid for PID {}", pid.0);
            let result = ffi::task_for_pid(mach_task_self(), pid.0 as c_int, &mut task);

            if result != KERN_SUCCESS {
                if result == libc::KERN_FAILURE {
                    let process_exists = libc::kill(pid.0 as libc::pid_t, 0) == 0;
                    // Only ESRCH proves the pid is gone: EPERM means the process exists but
                    // belongs to another user. Read errno before anything else can change it.
                    let no_such_process =
                        !process_exists && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);

                    if process_exists {
                        return Err(DebuggerError::PermissionDenied(format!(
                            "task_for_pid() failed with KERN_FAILURE, but process {} exists. This means insufficient \
                             permissions.\n\nQuick fix: Run with sudo:\n  sudo ferros attach {}\n\nAlternatively, use \
                             launch() to spawn processes under debugger control, which doesn't require special permissions.",
                            pid.0, pid.0
                        )));
                    }

                    if no_such_process {
                        return Err(DebuggerError::ProcessNotFound(pid.0));
                    }
                }

                return Err(DebuggerError::from(MachError::from(result)));
            }

            let mut thread_list: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;

            let result = task_threads(task, &mut thread_list, &mut thread_count);
            if result != KERN_SUCCESS || thread_count == 0 {
                threads::ThreadManager::deallocate_threads_array(thread_list, thread_count);
                let _ = ffi::mach_port_deallocate(mach_task_self(), task);
                return Err(DebuggerError::AttachFailed(format!("Failed to get threads: {}", result)));
            }

            let slice = std::slice::from_raw_parts(thread_list, thread_count as usize);
            self.task = task;
            self.pid = pid;
            self.threads = slice.to_vec();
            threads::ThreadManager::deallocate_threads_array(thread_list, thread_count);
        }

        *lock_shared(&self.exception_state) = ExceptionSharedState::new();
        self.memory_cache.clear();
        self.peeked = None;

        if let Err(err) = self.start_exception_handler() {
            self.release_ports();
            return Err(err);
        }

        Ok(())
    }

    /// Start the Mach exception handler thread.
    ///
    /// This creates a Mach receive port, registers it with `task_set_exception_ports()`,
    /// creates the stop event channel and spawns a background thread to handle exceptions
    /// (breakpoints, faults, etc.).
    ///
    /// ## Mach APIs Used
    ///
    /// - **mach_port_allocate()**: Creates a Mach receive port
    /// - **mach_port_insert_right()**: Makes the port sendable
    /// - **task_set_exception_ports()**: Registers the exception port with the task
    ///
    /// ## Exception Mask
    ///
    /// The handler receives:
    /// - `EXC_MASK_BREAKPOINT`: Software breakpoints (INT3/BRK)
    /// - `EXC_MASK_BAD_ACCESS`: Memory access violations
    /// - `EXC_MASK_BAD_INSTRUCTION`: Illegal instructions
    /// - `EXC_MASK_ARITHMETIC`: Arithmetic exceptions (div by zero, overflow)
    /// - `EXC_MASK_SOFTWARE`: Software-generated exceptions
    ///
    /// ## Errors
    ///
    /// Returns `MachError` if port allocation or exception port registration fails.
    /// Returns `AttachFailed` if the handler thread cannot be spawned.
    ///
    /// ## See Also
    ///
    /// - `stop_exception_handler()`: Stops the exception handler
    /// - `exception::run_exception_loop()`: The exception handling loop
    /// - [task_set_exception_ports(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_set_exception_ports/)
    fn start_exception_handler(&mut self) -> Result<()>
    {
        use tracing::info;

        self.stop_exception_handler();

        if self.task == MACH_PORT_NULL {
            return Ok(());
        }

        unsafe {
            let self_task = mach_task_self();
            let mut port: mach_port_t = MACH_PORT_NULL;
            let mut kr = mach_port_allocate(self_task, MACH_PORT_RIGHT_RECEIVE, &mut port);
            if kr != KERN_SUCCESS {
                return Err(DebuggerError::from(MachError::from(kr)));
            }

            kr = mach_port_insert_right(self_task, port, port, MACH_MSG_TYPE_MAKE_SEND);
            if kr != KERN_SUCCESS {
                let _ = mach_port_destroy(self_task, port);
                return Err(DebuggerError::from(MachError::from(kr)));
            }

            let mask: exception_mask_t = EXC_MASK_BREAKPOINT
                | EXC_MASK_BAD_ACCESS
                | EXC_MASK_BAD_INSTRUCTION
                | EXC_MASK_ARITHMETIC
                | EXC_MASK_SOFTWARE;
            let behavior: exception_behavior_t = (EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES) as exception_behavior_t;
            let flavor = exception::thread_state_flavor_for_arch(self.architecture);
            kr = task_set_exception_ports(self.task, mask, port, behavior, flavor);
            if kr != KERN_SUCCESS {
                let _ = mach_port_destroy(self_task, port);
                return Err(DebuggerError::from(MachError::from(kr)));
            }

            let (command_tx, command_rx) = mpsc::channel();
            let (events_tx, events_rx) = mpsc::channel();
            let loop_events = events_tx.clone();
            let shared_state = Arc::clone(&self.exception_state);
            let architecture = self.architecture;
            let task = self.task;
            info!("Spawning Mach exception handler thread");
            let handle = thread::Builder::new()
                .name("ferros-mac-exc".to_string())
                .spawn(move || {
                    exception::run_exception_loop(port, task, command_rx, shared_state, architecture, loop_events);
                })
                .map_err(|e| {
                    let _ = mach_port_destroy(self_task, port);
                    DebuggerError::AttachFailed(format!("Failed to spawn exception handler: {e}"))
                })?;

            self.exception_port = port;
            self.exception_thread = Some(handle);
            self.exception_commands = Some(command_tx);
            self.events_tx = Some(events_tx);
            self.events_rx = Some(events_rx);
        }

        Ok(())
    }

    /// Stop the exception handler thread.
    ///
    /// A held exception is replied to first, so its thread can continue once the task
    /// is resumed. Queued stop events are discarded.
    fn stop_exception_handler(&mut self)
    {
        if let Some(tx) = self.exception_commands.take() {
            let _ = tx.send(ExceptionLoopCommand::Shutdown);
        }

        if self.exception_port != MACH_PORT_NULL {
            unsafe {
                let _ = mach_port_destroy(mach_task_self(), self.exception_port);
            }
            self.exception_port = MACH_PORT_NULL;
        }

        if let Some(handle) = self.exception_thread.take() {
            let _ = handle.join();
        }

        self.events_tx = None;
        self.events_rx = None;
        self.peeked = None;
        lock_shared(&self.exception_state).pending_exception = false;
    }

    /// Balance every `task_suspend` the backend holds with `task_resume`, ignoring
    /// failures (used on teardown paths).
    fn release_suspends(&mut self)
    {
        let count = std::mem::take(&mut lock_shared(&self.exception_state).suspends);
        for _ in 0..count {
            unsafe {
                let _ = task_resume(self.task);
            }
        }
    }

    /// Deallocate the task and thread ports and forget the process.
    fn release_ports(&mut self)
    {
        use tracing::debug;

        unsafe {
            debug!("Deallocating {} thread ports", self.threads.len());
            for thread in &self.threads {
                let _ = ffi::mach_port_deallocate(mach_task_self(), *thread);
            }
            if self.task != 0 {
                debug!("Deallocating task port");
                let _ = ffi::mach_port_deallocate(mach_task_self(), self.task);
            }
        }

        self.task = 0;
        self.threads.clear();
        self.pid = ProcessId(0);
        self.launched = false;
        self.state = BackendState::NoProcess;
        self.memory_cache.clear();
        *lock_shared(&self.exception_state) = ExceptionSharedState::new();
    }

    /// Kill and reap a process this backend spawned but could not take control of.
    fn discard_child(pid: libc::pid_t)
    {
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
        Self::reap(pid);
    }

    /// Reap the child `pid`, waiting at most [`KILL_REAP_TIMEOUT`].
    fn reap(pid: libc::pid_t)
    {
        let deadline = Instant::now() + KILL_REAP_TIMEOUT;
        loop {
            let mut status: c_int = 0;
            let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if reaped != 0 || Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn invalid_state(&self, what: &str) -> BackendError
    {
        let state = match self.state {
            BackendState::NoProcess => "there is no process",
            BackendState::Stopped => "the target is stopped",
            BackendState::Running => "the target is running",
        };
        BackendError::InvalidState(format!("{what} is not allowed while {state}"))
    }

    fn require_stopped(&self, what: &str) -> BackendResult<()>
    {
        if self.state == BackendState::Stopped {
            Ok(())
        } else {
            Err(self.invalid_state(what))
        }
    }

    /// The port of `thread`, refreshing the thread list once if it is not known yet.
    fn thread_port(&mut self, thread: ThreadId) -> BackendResult<thread_act_t>
    {
        let port = thread_act_t::try_from(thread.raw())
            .map_err(|_| BackendError::NotFound(format!("thread {} does not exist", thread.raw())))?;
        if !self.threads.contains(&port) {
            self.threads = threads::ThreadManager::refresh_thread_list(self.task, &self.threads).map_err(backend_error)?;
        }
        if self.threads.contains(&port) {
            Ok(port)
        } else {
            Err(BackendError::NotFound(format!(
                "thread {} is not part of process {}",
                thread.raw(),
                self.pid.0
            )))
        }
    }

    /// Record that `event` was returned by `wait` and update the state.
    fn deliver(&mut self, event: StopEvent) -> StopEvent
    {
        self.memory_cache.clear();
        self.state = if event.is_terminal() {
            BackendState::NoProcess
        } else {
            BackendState::Stopped
        };
        event
    }

    /// Take a queued event without blocking.
    fn try_next_event(&mut self) -> Option<StopEvent>
    {
        self.peeked
            .take()
            .or_else(|| self.events_rx.as_ref().and_then(|rx| rx.try_recv().ok()))
    }

    /// Check `request` against what this backend supports.
    fn check_resume_request(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        let unsupported = |action: ThreadAction| match action {
            ThreadAction::Continue => None,
            ThreadAction::Step => Some(BackendError::Unsupported(
                "single-step is not implemented yet (T07, issue #33)".to_string(),
            )),
            ThreadAction::Suspend => Some(BackendError::Unsupported(
                "keeping individual threads suspended is not implemented yet".to_string(),
            )),
        };
        if !request.signals.is_empty() {
            return Err(BackendError::Unsupported(
                "passing signals on resume is not implemented yet (T06, issue #32)".to_string(),
            ));
        }
        if let Some(err) = unsupported(request.default_action) {
            return Err(err);
        }
        for (thread, action) in &request.thread_actions {
            self.thread_port(*thread)?;
            if let Some(err) = unsupported(*action) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Write `data` at `address`, making a read-only page writable for the write if needed.
    fn write_target_memory(&mut self, address: Address, data: &[u8]) -> Result<()>
    {
        let result = match write_memory(self.task, address, data) {
            Ok(_) => Ok(()),
            Err(first) => {
                // Code pages are not writable: make the range writable, write, and let the
                // guard restore the original protection.
                tracing::debug!(
                    "Direct write at 0x{:016x} failed ({first}); retrying with the page made writable",
                    address.value()
                );
                let guard = MemoryProtectionGuard::make_writable(self.task, address, data.len())?;
                let written = write_memory(self.task, address, data);
                drop(guard);
                written.map(|_| ())
            }
        };
        self.memory_cache.invalidate_range(address, data.len());
        result
    }
}

impl Backend for MacOSBackend
{
    /// Launch `spec.program` with `posix_spawn()` and `POSIX_SPAWN_START_SUSPENDED`, then
    /// take its task port.
    ///
    /// `spec.program` is argv\[0\], followed by `spec.args`; the environment, working
    /// directory and standard streams follow `spec`. If the task port cannot be obtained,
    /// the spawned process is killed and reaped before the error is returned.
    fn launch(&mut self, spec: &LaunchSpec) -> BackendResult<ProcessId>
    {
        use tracing::{debug, info};

        if self.state != BackendState::NoProcess {
            return Err(self.invalid_state("launch"));
        }

        let pid = launch::LaunchManager::launch(spec).map_err(backend_error)?;
        debug!("Attaching to spawned process");
        let process_id = ProcessId::from(pid as u32);
        if let Err(err) = self.attach_task(process_id) {
            Self::discard_child(pid);
            return Err(backend_error(err));
        }
        // The suspended start counts as the first suspend.
        lock_shared(&self.exception_state).suspends = 1;
        self.launched = true;
        self.state = BackendState::Stopped;
        info!("Successfully launched and attached to process {}", pid);
        Ok(process_id)
    }

    /// Attach with `task_for_pid()` and stop the target with `task_suspend()`.
    fn attach(&mut self, pid: ProcessId) -> BackendResult<()>
    {
        if self.state != BackendState::NoProcess {
            return Err(self.invalid_state("attach"));
        }

        self.attach_task(pid).map_err(backend_error)?;
        let kr = unsafe { task_suspend(self.task) };
        if kr != KERN_SUCCESS {
            self.stop_exception_handler();
            self.release_ports();
            return Err(mach_error(kr, "task_suspend failed"));
        }
        lock_shared(&self.exception_state).suspends = 1;
        self.launched = false;
        self.state = BackendState::Stopped;
        Ok(())
    }

    /// Shut down the exception handler (replying to a held exception), balance the task
    /// suspend count with `task_resume`, and release the ports. The target keeps running.
    fn detach(&mut self) -> BackendResult<()>
    {
        use tracing::info;

        self.require_stopped("detach")?;
        let alive = unsafe { libc::kill(self.pid.0 as libc::pid_t, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive {
            return Err(BackendError::NotFound(format!("process {} has exited", self.pid.0)));
        }

        let pid = self.pid.0;
        info!("Detaching from process {}", pid);
        self.stop_exception_handler();
        self.release_suspends();
        self.release_ports();
        info!("Successfully detached from process {}", pid);
        Ok(())
    }

    /// Minimal kill: `SIGKILL`, let a suspended task die, reap a launched process.
    fn kill(&mut self) -> BackendResult<()>
    {
        if self.state == BackendState::NoProcess {
            return Err(self.invalid_state("kill"));
        }

        let pid = self.pid.0 as libc::pid_t;
        let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
        if killed != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                return Err(BackendError::PermissionDenied(format!("cannot kill process {pid}: {err}")));
            }
            // ESRCH: already dead, which counts as success.
        }

        self.stop_exception_handler();
        self.release_suspends();
        if self.launched {
            Self::reap(pid);
        }
        self.release_ports();
        Ok(())
    }

    /// Receive the next stop event from the exception loop (or an interrupt).
    ///
    /// Known gap (T06, issue #32): nothing watches for process exit, so `Exited` and
    /// `Killed` are never reported, and `wait(None)` on a process that exits blocks.
    fn wait(&mut self, timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>
    {
        match self.state {
            BackendState::NoProcess => Err(self.invalid_state("wait")),
            BackendState::Stopped => match self.try_next_event() {
                Some(event) => Ok(Some(self.deliver(event))),
                None if timeout.is_none() => Err(BackendError::InvalidState(
                    "wait without a timeout while the target is stopped with no pending event would block forever"
                        .to_string(),
                )),
                None => Ok(None),
            },
            BackendState::Running => {
                if let Some(event) = self.peeked.take() {
                    return Ok(Some(self.deliver(event)));
                }
                let Some(rx) = self.events_rx.as_ref() else {
                    return Err(BackendError::Os {
                        code: -1,
                        message: "the exception handler is not running".to_string(),
                    });
                };
                let received = match timeout {
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                    Some(timeout) => rx.recv_timeout(timeout),
                };
                match received {
                    Ok(event) => Ok(Some(self.deliver(event))),
                    Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                    Err(mpsc::RecvTimeoutError::Disconnected) => Err(BackendError::Os {
                        code: -1,
                        message: "the exception handler stopped".to_string(),
                    }),
                }
            }
        }
    }

    /// Continue every thread: reply to the held exception, if any, then `task_resume`
    /// once per suspend the backend holds.
    ///
    /// Only [`ResumeRequest::continue_all`] is supported: `Step` (T07, issue #33),
    /// per-thread `Suspend` and signals (T06, issue #32) return
    /// [`BackendError::Unsupported`].
    fn resume(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        use tracing::info;

        self.require_stopped("resume")?;
        self.check_resume_request(request)?;
        if let Some(event) = self.try_next_event() {
            self.peeked = Some(event);
            return Err(BackendError::InvalidState(
                "resume is not allowed while stop events are pending; drain them with wait".to_string(),
            ));
        }

        let (count, pending) = {
            let mut shared = lock_shared(&self.exception_state);
            (std::mem::take(&mut shared.suspends), shared.pending_exception)
        };
        let restore = |backend: &Self, count: u32| lock_shared(&backend.exception_state).suspends += count;

        if pending {
            let (ack_tx, ack_rx) = mpsc::channel();
            let sent = self
                .exception_commands
                .as_ref()
                .is_some_and(|tx| tx.send(ExceptionLoopCommand::Continue(ack_tx)).is_ok());
            let replied = if sent {
                ack_rx
                    .recv_timeout(EXCEPTION_REPLY_TIMEOUT)
                    .unwrap_or_else(|_| Err("the exception handler did not answer".to_string()))
            } else {
                Err("the exception handler is not running".to_string())
            };
            if let Err(message) = replied {
                restore(self, count);
                return Err(BackendError::Os {
                    code: -1,
                    message: format!("failed to reply to the Mach exception: {message}"),
                });
            }
        }

        self.memory_cache.clear();
        for done in 0..count {
            let kr = unsafe { task_resume(self.task) };
            if kr != KERN_SUCCESS {
                // Re-suspend what was already resumed so the target stays stopped.
                for _ in 0..done {
                    unsafe {
                        let _ = task_suspend(self.task);
                    }
                }
                restore(self, count);
                return Err(mach_error(kr, "task_resume failed"));
            }
        }

        self.state = BackendState::Running;
        info!("Resumed process {}", self.pid.0);
        Ok(())
    }

    /// Stop a running target with `task_suspend()` and queue `Interrupted`.
    fn interrupt(&mut self) -> BackendResult<()>
    {
        match self.state {
            BackendState::NoProcess => Err(self.invalid_state("interrupt")),
            BackendState::Stopped => Ok(()),
            BackendState::Running => {
                {
                    let mut shared = lock_shared(&self.exception_state);
                    let kr = unsafe { task_suspend(self.task) };
                    if kr != KERN_SUCCESS {
                        return Err(mach_error(kr, "task_suspend failed"));
                    }
                    shared.suspends += 1;
                }
                if let Some(tx) = &self.events_tx {
                    let _ = tx.send(StopEvent::Interrupted { thread: None });
                }
                Ok(())
            }
        }
    }

    /// Enumerate the threads from the OS; a thread's id is its Mach thread port name.
    fn threads(&mut self) -> BackendResult<Vec<ThreadId>>
    {
        self.require_stopped("threads")?;
        self.threads = threads::ThreadManager::refresh_thread_list(self.task, &self.threads).map_err(backend_error)?;
        Ok(self.threads.iter().map(|&port| ThreadId::from(u64::from(port))).collect())
    }

    fn read_registers(&mut self, thread: ThreadId) -> BackendResult<Registers>
    {
        self.require_stopped("read_registers")?;
        let port = self.thread_port(thread)?;
        read_registers_for_port(self.architecture, port).map_err(backend_error)
    }

    fn write_registers(&mut self, thread: ThreadId, registers: &Registers) -> BackendResult<()>
    {
        self.require_stopped("write_registers")?;
        if registers.architecture() != self.architecture {
            return Err(BackendError::Unsupported(format!(
                "registers for {} cannot be written to a {} target",
                registers.architecture(),
                self.architecture
            )));
        }
        let port = self.thread_port(thread)?;
        write_registers_to_port(self.architecture, port, registers).map_err(backend_error)
    }

    fn read_memory(&mut self, address: Address, len: usize) -> BackendResult<Vec<u8>>
    {
        self.require_stopped("read_memory")?;
        let bytes = self.memory_cache.read(self.task, address, len).map_err(backend_error)?;
        if bytes.len() != len {
            return Err(BackendError::NotFound(format!(
                "only {} of {len} bytes at 0x{:016x} are mapped",
                bytes.len(),
                address.value()
            )));
        }
        Ok(bytes)
    }

    fn write_memory(&mut self, address: Address, data: &[u8]) -> BackendResult<()>
    {
        self.require_stopped("write_memory")?;
        self.write_target_memory(address, data).map_err(backend_error)
    }

    fn memory_regions(&mut self) -> BackendResult<Vec<MemoryRegion>>
    {
        self.require_stopped("memory_regions")?;
        get_memory_regions(self.task).map_err(backend_error)
    }

    /// The main executable only, for now (see [`crate::images`]).
    fn images(&mut self) -> BackendResult<Vec<LoadedImage>>
    {
        self.require_stopped("images")?;
        let regions = get_memory_regions(self.task).map_err(backend_error)?;
        images::loaded_images(self.task, self.pid, &regions).map_err(backend_error)
    }

    fn architecture(&self) -> Architecture
    {
        self.architecture
    }
}

impl Drop for MacOSBackend
{
    fn drop(&mut self)
    {
        if self.state == BackendState::NoProcess {
            self.stop_exception_handler();
            return;
        }
        if self.launched {
            let _ = Backend::kill(self);
        } else {
            self.stop_exception_handler();
            self.release_suspends();
            self.release_ports();
        }
    }
}
