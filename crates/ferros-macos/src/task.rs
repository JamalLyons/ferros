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
//! private attach SIGSTOP of a launch or attach, one per exception stop, and one per
//! interrupt. PT_ATTACHEXC consumes the suspended spawn before this private stop. The count lives in the shared exception state; `resume`,
//! `detach` and `kill` balance it with `task_resume`. Threads that stop at the same time
//! form one stop: one suspend, one event per thread.
//!
//! ## Per-thread actions
//!
//! `resume` honours every [`ThreadAction`]. Before the target runs it `thread_suspend`s
//! every thread whose action is `Suspend` and sets the hardware single-step bit
//! (`MDSCR_EL1.SS`, see [`crate::registers::set_single_step`]) of every thread whose
//! action is `Step`. Suspended threads retain their stop records and unanswered Unix
//! signal replies (see the exception module's "Held signals and retained thread
//! rights"). Hardware replies have already been sent. When `wait` delivers the next stop, the
//! whole task is `task_suspend`ed by the exception loop, so the backend balances its
//! `thread_suspend`s with `thread_resume` and clears the single-step bit of every thread
//! it asked to step: a step that did not complete is cancelled, as the contract says,
//! and a later plain continue cannot single-step by surprise. Every teardown path
//! (`detach`, `kill`, drop, the rollback of a failed `resume`) balances them too;
//! `detach` does it before it raises its private SIGSTOP, so that signal cannot be
//! taken by a thread that is still suspended.
//!
//! ## Exception ports
//!
//! Attaching installs the backend's handler with `task_swap_exception_ports` and keeps
//! the ports it replaced; `detach` (and dropping an attached backend, and a failed
//! attach) puts them back before the handler port is destroyed, so the target ends up
//! with the exception ports it had before.
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
use mach2::task::{task_resume, task_suspend, task_threads};
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;

use crate::error::MachError;
use crate::exception::{
    self, ExceptionLoopCommand, ExceptionSharedState, LoopEvent, ReplyFaultInjector, SavedExceptionPorts, lock_shared,
};
use crate::memory::{MemoryCache, MemoryProtectionGuard, get_memory_regions, write_memory};
#[cfg(target_arch = "aarch64")]
use crate::registers::{read_registers_arm64, write_registers_arm64};
#[cfg(target_arch = "x86_64")]
use crate::registers::{read_registers_x86_64, write_registers_x86_64};
use crate::{ffi, images, launch, threads};

/// How long `resume` waits for the exception loop to confirm its reply.
const EXCEPTION_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long detach waits for the private SIGSTOP to take effect before continuing the
/// released process.
const DETACH_STOP_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// Whether PT_ATTACHEXC has established tracing.
    traced: bool,
    /// Sole owner of waitpid while tracing.
    exit_watcher: Option<crate::lifecycle::ExitWatcher>,
    /// Mach exception port used to receive stop notifications.
    exception_port: mach_port_t,
    /// Thread running the Mach exception handling loop.
    exception_thread: Option<thread::JoinHandle<()>>,
    /// Channel used to command the exception loop (continue/shutdown).
    exception_commands: Option<mpsc::Sender<ExceptionLoopCommand>>,
    /// Suspend count and held exception, shared with the exception loop.
    exception_state: Arc<Mutex<ExceptionSharedState>>,
    /// The target's exception ports from before the handler was installed.
    saved_exception_ports: Option<SavedExceptionPorts>,
    /// Test hook shared with the exception loop (see `fail_next_exception_reply`).
    reply_faults: ReplyFaultInjector,
    /// Sender side of the stop event channel (the exception loop holds a clone).
    events_tx: Option<mpsc::Sender<LoopEvent>>,
    /// Receiver side of the stop event channel, read by `wait`.
    events_rx: Option<mpsc::Receiver<LoopEvent>>,
    /// An event taken off the channel by `resume` while checking for pending events.
    peeked: Option<LoopEvent>,
    /// Cached memory pages for repeated reads; cleared whenever the target may run.
    memory_cache: MemoryCache,
    /// Threads the last resume `thread_suspend`ed (action `Suspend`), each once; balanced
    /// with `thread_resume` when the next stop is delivered and on every teardown path.
    suspended_threads: Vec<thread_act_t>,
    /// Threads whose single-step bit the last resume set (action `Step`); the bit is
    /// cleared when the next stop is delivered and on teardown.
    stepping_threads: Vec<thread_act_t>,
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
            traced: false,
            exit_watcher: None,
            exception_port: MACH_PORT_NULL,
            exception_thread: None,
            exception_commands: None,
            exception_state: Arc::new(Mutex::new(ExceptionSharedState::new())),
            saved_exception_ports: None,
            reply_faults: ReplyFaultInjector::default(),
            events_tx: None,
            events_rx: None,
            peeked: None,
            memory_cache: MemoryCache::new(),
            suspended_threads: Vec::new(),
            stepping_threads: Vec::new(),
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
    /// This bypasses the backend's state tracking: [`Backend::resume`] with
    /// [`ThreadAction::Suspend`] is the supported way to keep threads suspended.
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

    /// Test hook: make the next exception reply report a failure.
    ///
    /// The reply is still sent so the thread is not stranded, but `wait` reports an
    /// injected `MACH_SEND_INVALID_DEST`. Hardware replies precede the stop event;
    /// drain the stop and then the error before resuming. Unix signal replies remain
    /// deferred until resume. An error from `wait` leaves the state unchanged.
    #[doc(hidden)]
    pub fn fail_next_exception_reply(&mut self)
    {
        self.reply_faults.fail_next_exception_reply();
    }

    /// Test hook: a handle that calls
    /// [`fail_next_exception_reply`](Self::fail_next_exception_reply) on this backend
    /// after it has been moved into a `ferros_core::Session`.
    #[doc(hidden)]
    #[must_use]
    pub fn reply_fault_injector(&self) -> ReplyFaultInjector
    {
        self.reply_faults.clone()
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
            self.stop_exception_handler(true);
            self.release_ports();
            return Err(err);
        }

        Ok(())
    }

    /// Start the Mach exception handler thread.
    ///
    /// This creates a Mach receive port, registers it with `task_swap_exception_ports()`
    /// (saving the ports it replaces for [`stop_exception_handler`](Self::stop_exception_handler)
    /// to restore), creates the stop event channel and spawns a background thread to handle
    /// exceptions (breakpoints, faults, etc.).
    ///
    /// ## Mach APIs Used
    ///
    /// - **mach_port_allocate()**: Creates a Mach receive port
    /// - **mach_port_insert_right()**: Makes the port sendable
    /// - **task_swap_exception_ports()**: Registers the exception port with the task and
    ///   returns the previous ones
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

        self.stop_exception_handler(true);

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
            let saved = match SavedExceptionPorts::install(self.task, mask, port, behavior, flavor) {
                Ok(saved) => saved,
                Err(kr) => {
                    let _ = mach_port_destroy(self_task, port);
                    return Err(DebuggerError::from(MachError::from(kr)));
                }
            };
            // From here on, `stop_exception_handler` restores the saved ports and destroys
            // the port, including when spawning the thread fails.
            self.saved_exception_ports = Some(saved);
            self.exception_port = port;

            let (command_tx, command_rx) = mpsc::channel();
            let (events_tx, events_rx) = mpsc::channel();
            let loop_events = events_tx.clone();
            let shared_state = Arc::clone(&self.exception_state);
            let architecture = self.architecture;
            let task = self.task;
            let faults = self.reply_faults.clone();
            info!("Spawning Mach exception handler thread");
            let handle = thread::Builder::new()
                .name("ferros-mac-exc".to_string())
                .spawn(move || {
                    exception::run_exception_loop(port, task, command_rx, shared_state, architecture, loop_events, faults);
                })
                .map_err(|e| DebuggerError::AttachFailed(format!("Failed to spawn exception handler: {e}")))?;

            self.exception_thread = Some(handle);
            self.exception_commands = Some(command_tx);
            self.events_tx = Some(events_tx);
            self.events_rx = Some(events_rx);
        }

        Ok(())
    }

    /// Stop the exception handler thread.
    ///
    /// With `restore`, the target's saved exception ports are put back first, so new
    /// exceptions go to their old handlers; without it (the process is being killed) the
    /// saved rights are only released. Held exceptions are then replied to, so their
    /// threads can continue once the task is resumed. Queued stop events are discarded.
    fn stop_exception_handler(&mut self, restore: bool)
    {
        if let Some(saved) = self.saved_exception_ports.take() {
            if restore && self.task != MACH_PORT_NULL {
                saved.restore(self.task);
            }
            drop(saved);
        }

        // If the loop holds exceptions it is waiting for a command: let it reply, answer
        // the exceptions still queued on the port and exit before the port is destroyed.
        // Otherwise it is blocked receiving, and destroying the port is what wakes it.
        let holds_exception = lock_shared(&self.exception_state).pending_exception;
        if let Some(tx) = self.exception_commands.take() {
            let _ = tx.send(ExceptionLoopCommand::Shutdown);
        }
        if holds_exception && let Some(handle) = self.exception_thread.take() {
            let _ = handle.join();
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
        // The loop answers what it holds before it exits; this only catches exceptions
        // left behind by a loop that never ran.
        exception::release_held_exceptions(&mut lock_shared(&self.exception_state));
    }

    /// Balance every `thread_suspend` of the last resume with `thread_resume`.
    ///
    /// A failure (the thread has exited, or the process is gone) is ignored.
    fn release_thread_suspends(&mut self)
    {
        for port in std::mem::take(&mut self.suspended_threads) {
            // SAFETY: `port` is a thread right this backend holds (it is in `threads`);
            // the call only balances the suspend this backend added.
            let kr = unsafe { ffi::thread_resume(port) };
            if kr != KERN_SUCCESS {
                tracing::debug!("thread_resume({port}) failed with {kr}; the thread may have exited");
            }
        }
    }

    /// Clear the single-step bit of every thread the last resume asked to step.
    ///
    /// Clearing an already clear bit (the kernel clears it when the step completes) is
    /// harmless; a failure (the thread has exited) is ignored.
    fn clear_step_bits(&mut self)
    {
        for port in std::mem::take(&mut self.stepping_threads) {
            if let Err(err) = crate::registers::set_single_step(port, false) {
                tracing::debug!("clearing the single-step bit of thread {port} failed: {err}");
            }
        }
    }

    /// The threads `request` suspends and steps, in that order.
    ///
    /// A request whose actions are all `Continue` names none and needs no thread list;
    /// otherwise the list is refreshed, so the default action covers every current thread.
    fn plan_thread_actions(&mut self, request: &ResumeRequest) -> BackendResult<(Vec<thread_act_t>, Vec<thread_act_t>)>
    {
        let all_continue = request.default_action == ThreadAction::Continue
            && request
                .thread_actions
                .iter()
                .all(|(_, action)| *action == ThreadAction::Continue);
        if all_continue {
            return Ok((Vec::new(), Vec::new()));
        }
        self.threads = threads::ThreadManager::refresh_thread_list(self.task, &self.threads).map_err(backend_error)?;
        // `check_resume_request` checked the named threads against the cached list; a
        // thread that has exited since is gone from the refreshed one. Planning without it
        // would suspend every other thread with nothing left to step or run.
        if let Some((gone, _)) = request
            .thread_actions
            .iter()
            .find(|(thread, _)| !self.threads.iter().any(|&port| u64::from(port) == thread.raw()))
        {
            return Err(BackendError::NotFound(format!(
                "thread {} is not part of process {}",
                gone.raw(),
                self.pid.0
            )));
        }
        let mut suspend = Vec::new();
        let mut step = Vec::new();
        for &port in &self.threads {
            match request.action_for(ThreadId::from(u64::from(port))) {
                ThreadAction::Continue => {}
                ThreadAction::Suspend => suspend.push(port),
                ThreadAction::Step => step.push(port),
            }
        }
        Ok((suspend, step))
    }

    /// `thread_suspend` every thread in `suspend` and set the single-step bit of every
    /// thread in `step`, recording both. On failure everything done is undone and the
    /// error returned.
    fn apply_thread_actions(&mut self, suspend: &[thread_act_t], step: &[thread_act_t]) -> BackendResult<()>
    {
        // Normally balanced already, when the stop that made the target *Stopped* was
        // delivered.
        self.release_thread_suspends();
        self.clear_step_bits();
        for &port in suspend {
            // SAFETY: `port` is a thread right this backend holds; the suspend is recorded
            // and balanced exactly once by `release_thread_suspends`.
            let kr = unsafe { ffi::thread_suspend(port) };
            if kr != KERN_SUCCESS {
                self.release_thread_suspends();
                return Err(mach_error(kr, &format!("thread_suspend of thread {port} failed")));
            }
            self.suspended_threads.push(port);
        }
        for &port in step {
            self.stepping_threads.push(port);
            if let Err(err) = crate::registers::set_single_step(port, true) {
                self.clear_step_bits();
                self.release_thread_suspends();
                return Err(backend_error(err));
            }
        }
        Ok(())
    }

    /// Undo [`apply_thread_actions`](Self::apply_thread_actions) (the rollback of a failed
    /// resume, and teardown).
    fn undo_thread_actions(&mut self)
    {
        self.release_thread_suspends();
        self.clear_step_bits();
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
    ///
    /// Thread suspends and single-step bits still recorded are balanced first, while the
    /// thread ports are still held (failures, for a dead process, are ignored).
    fn release_ports(&mut self)
    {
        use tracing::debug;

        self.undo_thread_actions();

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
        self.traced = false;
        self.state = BackendState::NoProcess;
        self.memory_cache.clear();
        *lock_shared(&self.exception_state) = ExceptionSharedState::new();
    }

    /// Kill and reap a process this backend spawned but could not take control of.
    fn discard_child(pid: libc::pid_t) -> BackendResult<()>
    {
        // SAFETY: posix_spawn returned our unreaped child; its pid cannot be reused.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(backend_error(err.into()));
            }
        }
        launch::reap_child(pid, KILL_REAP_TIMEOUT)
    }

    /// Establish signal tracing and wait for its private SIGSTOP exception. Darwin's
    /// PT_ATTACHEXC consumes a pre-existing task suspend (including suspended spawn).
    fn start_tracing(&mut self) -> BackendResult<()>
    {
        let (ready, wait) = mpsc::channel();
        lock_shared(&self.exception_state).internal_stop = Some(ready);
        self.ptrace(ffi::ptrace_requests::PT_ATTACHEXC, 0, 0)?;
        self.traced = true;
        wait.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
            code: -1,
            message: "ptrace attach SIGSTOP did not arrive within 5s".into(),
        })?;
        // Suppress and answer the attach signal while retaining the task suspend.
        // Holding this exception indefinitely would defer an external SIGKILL.
        let ports = lock_shared(&self.exception_state).soft_signals.clone();
        for port in ports {
            self.ptrace(ffi::ptrace_requests::PT_THUPDATE, port, 0)?;
        }
        let (ack, replied) = mpsc::channel();
        self.exception_commands
            .as_ref()
            .expect("handler started")
            .send(ExceptionLoopCommand::Continue { ack, keep: Vec::new() })
            .map_err(|_| BackendError::Os {
                code: -1,
                message: "attach reply channel closed".into(),
            })?;
        replied.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
            code: -1,
            message: "attach reply timed out".into(),
        })?;
        self.exit_watcher = Some(crate::lifecycle::ExitWatcher::start(
            self.pid.0 as libc::pid_t,
            Arc::clone(&self.exception_state),
            self.events_tx.as_ref().expect("handler started").clone(),
        )?);
        Ok(())
    }

    /// Issue one Darwin tracing request; the address encodes a thread port, never memory.
    fn ptrace(&self, request: c_int, thread: thread_act_t, signal: c_int) -> BackendResult<()>
    {
        // SAFETY: this backend owns the pid's tracing relationship (or is attaching);
        // Darwin interprets the address as a Mach port name for PT_THUPDATE only.
        let result = unsafe {
            ffi::ptrace(
                request,
                self.pid.0 as libc::pid_t,
                thread as usize as *mut libc::c_char,
                signal,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        Err(backend_error(DebuggerError::Io(err)))
    }

    /// End the trace: PT_DETACH requires the tracee to wait in a signal exception, not
    /// just a task_suspend, so the backend raises a private SIGSTOP and detaches while its
    /// exception is held. Answering that exception afterwards stops the process for real;
    /// [`detach_target`](Self::detach_target) continues it once the reply is sent.
    fn stop_tracing(&mut self) -> BackendResult<()>
    {
        if !self.traced {
            return Ok(());
        }
        // Always end the trace from the private SIGSTOP. If a user signal is held, the
        // resume below suppresses it (detach discards held signals) before the SIGSTOP
        // is taken; delivering it after detach would, for SIGUSR1, kill the target.
        let (ready, wait) = mpsc::channel();
        {
            let mut shared = lock_shared(&self.exception_state);
            if shared.dead {
                return Err(BackendError::NotFound(format!("process {} has exited", self.pid.0)));
            }
            shared.internal_stop = Some(ready);
            // SAFETY: the lock excludes reaping until after this signal.
            if unsafe { libc::kill(self.pid.0 as libc::pid_t, libc::SIGSTOP) } != 0 {
                return Err(backend_error(DebuggerError::Io(std::io::Error::last_os_error())));
            }
        }
        if self.state == BackendState::Stopped {
            while self.try_next_event().is_some() {}
            self.resume(&ResumeRequest::continue_all())?;
        }
        wait.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
            code: -1,
            message: "detach SIGSTOP did not arrive within 5s".into(),
        })?;
        // PT_DETACH needs the tracee waiting in its signal exception (EBUSY otherwise),
        // so it comes before the reply.
        self.ptrace(ffi::ptrace_requests::PT_DETACH, 0, 0)?;
        self.traced = false;
        Ok(())
    }

    /// Shut down tracing, restore exception handlers and balance our suspensions.
    ///
    /// The teardown always runs, even when ending the trace fails (for example the
    /// private SIGSTOP never arrives because another stop won the race): skipping it
    /// would leak the handler thread, its port and the task rights and leave the target
    /// suspended. In that case `PT_DETACH` is still attempted and the first error is
    /// returned afterwards.
    fn detach_target(&mut self) -> BackendResult<()>
    {
        // Before the private SIGSTOP: the kernel could otherwise pick a thread that is
        // still `thread_suspend`ed to take it, and the detach would time out. No thread
        // of a released process may be left single-stepping either.
        self.undo_thread_actions();
        let traced = self.stop_tracing();
        if traced.is_err() && self.traced {
            let _ = self.ptrace(ffi::ptrace_requests::PT_DETACH, 0, 0);
            self.traced = false;
        }
        self.exit_watcher = None;
        self.stop_exception_handler(true);
        self.release_suspends();
        if traced.is_ok() {
            // The handler answered the private SIGSTOP exception after PT_DETACH, so the
            // kernel takes the SIGSTOP as an ordinary job-control stop (state `T`) once the
            // thread processes the reply. Wait (bounded) until it has, so the SIGCONT
            // cannot overtake the stop, then undo our own stop: the released process must
            // keep running. A program with a SIGCONT handler sees one extra SIGCONT.
            let pid = self.pid.0 as libc::pid_t;
            let deadline = Instant::now() + DETACH_STOP_TIMEOUT;
            while !crate::lifecycle::is_job_control_stopped(pid) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            // SAFETY: plain kill(2) on the pid this backend still holds (not yet released).
            if unsafe { libc::kill(self.pid.0 as libc::pid_t, libc::SIGCONT) } != 0 {
                tracing::warn!(
                    "SIGCONT after detach failed: {}; process {} may stay stopped",
                    std::io::Error::last_os_error(),
                    self.pid.0
                );
            }
        }
        self.release_ports();
        traced
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
    ///
    /// A thread the event names that is not in the thread list yet (it started after the
    /// last enumeration) is added now, while the exception loop still holds the message's
    /// right to it: the backend's own right keeps the port name, which is the
    /// [`ThreadId`], valid after the loop releases its right on resume.
    ///
    /// A non-terminal stop ends the resume before it: the whole task is `task_suspend`ed,
    /// so the `thread_suspend`s of that resume are balanced now, and the single-step bit
    /// of every thread it asked to step is cleared (cancelling a step that did not
    /// complete).
    fn deliver(&mut self, event: StopEvent) -> StopEvent
    {
        self.memory_cache.clear();
        if !event.is_terminal() {
            self.undo_thread_actions();
        }
        if let Some(thread) = event.thread()
            && let Ok(port) = thread_act_t::try_from(thread.raw())
            && !self.threads.contains(&port)
            && let Ok(threads) = threads::ThreadManager::refresh_thread_list(self.task, &self.threads)
        {
            self.threads = threads;
        }
        if event.is_terminal() {
            self.exit_watcher = None;
            self.stop_exception_handler(false);
            self.release_ports();
        } else {
            self.state = BackendState::Stopped;
        }
        event
    }

    /// Take a queued event (or queued failure) without blocking.
    fn try_next_event(&mut self) -> Option<LoopEvent>
    {
        self.peeked
            .take()
            .or_else(|| self.events_rx.as_ref().and_then(|rx| rx.try_recv().ok()))
    }

    /// Check `request` against what this backend supports.
    ///
    /// Every action is supported. A signal must name a thread holding a soft-signal
    /// exception that does not stay suspended (`Unsupported` otherwise), and a thread
    /// named in `thread_actions` must exist (`NotFound` otherwise).
    fn check_resume_request(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        for (thread, signal) in &request.signals {
            if request.action_for(*thread) == ThreadAction::Suspend {
                return Err(BackendError::Unsupported(format!(
                    "a signal cannot be passed to thread {} while it stays suspended",
                    thread.raw()
                )));
            }
            if !lock_shared(&self.exception_state)
                .soft_signals
                .iter()
                .any(|&port| thread.raw() == u64::from(port))
            {
                return Err(BackendError::Unsupported(
                    "signal injection requires a held soft-signal exception".into(),
                ));
            }
            if !(0..32).contains(signal) {
                return Err(BackendError::Unsupported(format!("invalid signal {signal}")));
            }
        }
        for (thread, _) in &request.thread_actions {
            self.thread_port(*thread)?;
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
            Self::discard_child(pid).map_err(|cleanup| BackendError::Os {
                code: -1,
                message: format!("launch failed ({err}); child cleanup also failed: {cleanup}"),
            })?;
            return Err(backend_error(err));
        }
        self.launched = true;
        self.state = BackendState::Stopped;
        if let Err(err) = self.start_tracing() {
            self.kill().map_err(|cleanup| BackendError::Os {
                code: -1,
                message: format!("tracing setup failed ({err}); child cleanup also failed: {cleanup}"),
            })?;
            return Err(err);
        }
        info!("Successfully launched and attached to process {}", pid);
        Ok(process_id)
    }

    /// Attach with `task_for_pid()` and PT_ATTACHEXC. Its private SIGSTOP
    /// establishes the initial whole-task suspension and is suppressed internally.
    fn attach(&mut self, pid: ProcessId) -> BackendResult<()>
    {
        if self.state != BackendState::NoProcess {
            return Err(self.invalid_state("attach"));
        }

        self.attach_task(pid).map_err(backend_error)?;
        self.state = BackendState::Stopped;
        if let Err(err) = self.start_tracing() {
            // Before tracing succeeds no SIGSTOP was sent; afterwards undo tracing
            // before restoring ports, so no signal can strand the target.
            if self.traced {
                let _ = self.detach_target();
            } else {
                self.stop_exception_handler(true);
                self.release_suspends();
                self.release_ports();
            }
            return Err(err);
        }
        Ok(())
    }

    /// Restore the target's exception ports, shut down the exception handler (replying to
    /// held and queued exceptions), balance the task suspend count with `task_resume`, and
    /// release the ports. The target keeps running.
    fn detach(&mut self) -> BackendResult<()>
    {
        use tracing::info;

        self.require_stopped("detach")?;
        let alive = !lock_shared(&self.exception_state).dead && !crate::lifecycle::task_is_dead(self.task);
        if !alive {
            return Err(BackendError::NotFound(format!("process {} has exited", self.pid.0)));
        }

        let pid = self.pid.0;
        info!("Detaching from process {}", pid);
        self.detach_target()?;
        info!("Successfully detached from process {}", pid);
        Ok(())
    }

    /// SIGKILL works at Mach and ptrace stops; release holds and await the sole reaper.
    fn kill(&mut self) -> BackendResult<()>
    {
        if self.state == BackendState::NoProcess {
            return Err(self.invalid_state("kill"));
        }

        let pid = self.pid.0 as libc::pid_t;
        {
            let shared = lock_shared(&self.exception_state);
            // The watcher marks death before it can reap, under this lock. Never
            // signal a remembered pid once reaping may have allowed pid reuse.
            if !shared.dead {
                // SAFETY: this tracee is not yet reaped; kill does not access memory.
                let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
                if killed != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::ESRCH) {
                        return Err(backend_error(DebuggerError::Io(err)));
                    }
                }
            }
        }

        // The process is dying: its exception ports need no restoring, only releasing.
        // Threads kept suspended by a resume in flight are let go too, so nothing of
        // ours holds a thread back from exiting.
        self.release_thread_suspends();
        self.stop_exception_handler(false);
        self.release_suspends();
        if let Some(watcher) = &self.exit_watcher {
            watcher.wait(KILL_REAP_TIMEOUT)?;
        } else if self.launched {
            launch::reap_child(pid, KILL_REAP_TIMEOUT)?;
        }
        self.exit_watcher = None;
        self.release_ports();
        Ok(())
    }

    /// Receive the next stop event from the exception loop (or an interrupt).
    ///
    /// An exception reply failure is returned once as `BackendError::Os` (the state
    /// does not change), and later calls report later stops as usual.
    ///
    /// The kqueue watcher publishes terminal events after waitpid has collected status.
    fn wait(&mut self, timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>
    {
        if self.state == BackendState::NoProcess {
            return Err(self.invalid_state("wait"));
        }
        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            let queued = self.try_next_event();
            let dead = lock_shared(&self.exception_state).dead || crate::lifecycle::task_is_dead(self.task);
            if queued.is_none() && self.state == BackendState::Stopped && timeout.is_none() && !dead {
                return Err(BackendError::InvalidState(
                    "wait without a timeout while stopped with no pending event would block forever".into(),
                ));
            }
            let received = match queued {
                Some(event) => Ok(event),
                None => {
                    let rx = self.events_rx.as_ref().ok_or_else(|| BackendError::Os {
                        code: -1,
                        message: "the exception handler is not running".into(),
                    })?;
                    match deadline {
                        None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    }
                }
            };
            match received {
                Ok(Ok(event)) if event.is_terminal() => return Ok(Some(self.deliver(event))),
                // A terminal event takes precedence over stale stops and reply failures
                // from dead threads, even if those reached the channel first.
                Ok(_) if dead || crate::lifecycle::task_is_dead(self.task) => continue,
                Ok(Ok(event)) => return Ok(Some(self.deliver(event))),
                Ok(Err(err)) => return Err(err),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(BackendError::Os {
                        code: -1,
                        message: "the event channel stopped".into(),
                    });
                }
            }
        }
    }

    /// Resume the target as `request` describes, in this order:
    ///
    /// 1. `thread_suspend` every thread whose action is `Suspend` and set the single-step
    ///    bit (`MDSCR_EL1.SS`) of every thread whose action is `Step`;
    /// 2. release runnable threads' stop records and answer their Unix signal replies;
    ///    suspended threads retain their records and held signals;
    /// 3. `task_resume` once per suspend the backend holds.
    ///
    /// A step that completes is reported by `wait` as [`StopEvent::SingleStep`]. Whatever
    /// stop `wait` reports next, the `thread_suspend`s are then balanced and every
    /// single-step bit set here is cleared (see `deliver`).
    ///
    /// A reply that fails does not fail the resume: the next `wait` reports it. If any
    /// other step fails, what was done is undone (`thread_resume`, single-step bits
    /// cleared, the suspend count restored) and the target stays *Stopped*.
    ///
    /// Held Unix signals are suppressed unless named in `request.signals`. A signal named
    /// for a thread whose action is `Suspend`, or for a thread without a held soft-signal
    /// exception, returns [`BackendError::Unsupported`]; so does `Step` on x86-64.
    fn resume(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        use tracing::info;

        self.require_stopped("resume")?;
        if lock_shared(&self.exception_state).dead || crate::lifecycle::task_is_dead(self.task) {
            return Err(BackendError::InvalidState(
                "process death is pending; drain it with wait".into(),
            ));
        }
        self.check_resume_request(request)?;
        let (suspend, step) = self.plan_thread_actions(request)?;
        // The exception loop queues a stop event under this lock, together with its
        // suspend and the held-exception flag. Checking the queue under the same lock
        // means a stop is either fully visible here (and the resume is refused) or has
        // not happened yet.
        let shared_state = Arc::clone(&self.exception_state);
        let (count, pending, held) = {
            let mut shared = lock_shared(&shared_state);
            if let Some(event) = self.try_next_event() {
                self.peeked = Some(event);
                return Err(BackendError::InvalidState(
                    "resume is not allowed while stop events are pending; drain them with wait".to_string(),
                ));
            }
            (
                std::mem::take(&mut shared.suspends),
                shared.pending_exception,
                !shared.held.is_empty(),
            )
        };
        let restore = |backend: &mut Self, count: u32| {
            backend.undo_thread_actions();
            lock_shared(&backend.exception_state).suspends += count;
        };

        // Suspend and arm the threads before any of them can run.
        if let Err(err) = self.apply_thread_actions(&suspend, &step) {
            restore(self, count);
            return Err(err);
        }

        if pending || held {
            // Only the exceptions answered now get their signal decided; a kept one (its
            // thread stays suspended) keeps its signal held for a later resume.
            let signals = lock_shared(&self.exception_state).soft_signals.clone();
            for port in signals.into_iter().filter(|port| !suspend.contains(port)) {
                let signal = request.signal_for(ThreadId::from(u64::from(port))).unwrap_or(0);
                if let Err(err) = self.ptrace(ffi::ptrace_requests::PT_THUPDATE, port, signal) {
                    restore(self, count);
                    return Err(err);
                }
            }
        }
        if !pending && held {
            // Only kept exceptions are held and the loop is blocked receiving: answer
            // those of the threads that run now from here, under the lock.
            if let Some(events) = self.events_tx.clone() {
                let mut shared = lock_shared(&self.exception_state);
                exception::answer_held_exceptions(&mut shared, &suspend, &self.reply_faults, &events, self.task);
            }
        }
        if pending {
            let (ack_tx, ack_rx) = mpsc::channel();
            let keep = suspend.clone();
            let sent = self
                .exception_commands
                .as_ref()
                .is_some_and(|tx| tx.send(ExceptionLoopCommand::Continue { ack: ack_tx, keep }).is_ok());
            let replied = if sent {
                ack_rx
                    .recv_timeout(EXCEPTION_REPLY_TIMEOUT)
                    .map_err(|_| "the exception handler did not answer")
            } else {
                Err("the exception handler is not running")
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
                // Delivering a fatal soft signal can destroy the task as soon as its
                // exception is answered, before task_resume reaches the kernel. Resume
                // succeeded; wait owns the pending terminal event.
                // The task can be inactive before its port becomes a dead name or
                // NOTE_EXIT is published. Give the watcher a bounded chance to confirm.
                let deadline = Instant::now() + Duration::from_millis(200);
                loop {
                    if lock_shared(&self.exception_state).dead || crate::lifecycle::task_is_dead(self.task) {
                        self.state = BackendState::Running;
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
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
                    // An exception stop is already queued for `wait`: the interrupt lost
                    // the race and is consumed, as the contract requires.
                    if shared.pending_exception {
                        return Ok(());
                    }
                    let kr = unsafe { task_suspend(self.task) };
                    if kr != KERN_SUCCESS {
                        return Err(mach_error(kr, "task_suspend failed"));
                    }
                    shared.suspends += 1;
                    if let Some(tx) = &self.events_tx {
                        let _ = tx.send(Ok(StopEvent::Interrupted { thread: None }));
                    }
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
            self.exit_watcher = None;
            self.stop_exception_handler(false);
            self.release_ports();
            return;
        }
        if self.launched {
            let _ = Backend::kill(self);
        } else if lock_shared(&self.exception_state).dead || crate::lifecycle::task_is_dead(self.task) {
            // Death may be observed before the status becomes waitable. Let the
            // watcher finish handing the tracee back/reaping before cancelling it.
            if let Some(watcher) = &self.exit_watcher {
                let _ = watcher.wait(KILL_REAP_TIMEOUT);
            }
            self.exit_watcher = None;
            self.stop_exception_handler(false);
            self.release_ports();
        } else if let Err(err) = self.detach_target() {
            tracing::error!("backend drop could not detach: {err}");
        }
    }
}
