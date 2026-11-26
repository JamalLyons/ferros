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
//! action is `Step`. Suspended threads retain their stop records and Unix
//! signal decisions (see the exception module's "Held signals and retained thread
//! rights"). Client exception replies have already been sent. When `wait` delivers the next stop, the
//! whole task is `task_suspend`ed by the exception loop, so the backend balances its
//! `thread_suspend`s with `thread_resume` and clears the single-step bit of every thread
//! it asked to step: a step that did not complete is cancelled, as the contract says,
//! and a later plain continue cannot single-step by surprise. Every teardown path
//! (`detach`, `kill`, drop, the rollback of a failed `resume`) balances them too;
//! `detach` does it before it raises its private SIGSTOP, so that signal cannot be
//! taken by a thread that is still suspended.
//!
//! When the default action is `Suspend` and some thread will run, a thread created
//! during that resume is held at the first instruction of `libsystem_pthread`'s
//! `_thread_start` or `_start_wqthread` until a later resume lets it execute that
//! instruction. Continuing every thread removes those traps first. If dyld has not
//! published the library yet, the resume breakpoints the executable entry and
//! arms the pthread entries there, before that instruction runs. Failing to
//! install a required trap fails the resume. While a held thread executes the
//! restored entry instruction, every other thread stays suspended, including
//! across a requested single-step of that thread. A software breakpoint on a
//! pthread entry is reported and the newborn stays suspended. `read_memory` shows the
//! instructions under these traps and an engine breakpoint written there survives
//! their removal. At an armed entry with no software breakpoint, the backend emulates
//! the entry instruction for a reused workqueue thread told to `Continue`. If emulation
//! fails, the backend holds that thread and [`Backend::wait`] reports the error in its
//! original category.
//! The process remains running, including when emulation returns `BackendError::NotFound`.
//! `ThreadAction::Step` as the default, and any thread started at another address, are
//! outside this guarantee. On x86-64 such a resume is `BackendError::Unsupported`.
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

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ferros_core::arch::{Arch, Arm64, UndefinedReturnAddress};
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
use crate::guards::ThreadSuspendGuard;
use crate::memory::{MemoryCache, get_memory_regions, write_memory_all_or_nothing};
#[cfg(target_arch = "aarch64")]
use crate::registers::{read_registers_arm64, write_registers_arm64};
#[cfg(target_arch = "x86_64")]
use crate::registers::{read_registers_x86_64, write_registers_x86_64};
use crate::{ffi, images, launch, threads};

/// How long `resume` waits for the exception loop to confirm its reply.
const EXCEPTION_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why [`MacOSBackend::resume_task`] failed.
enum ResumeTaskError
{
    /// Every suspend was put back; the task is stopped as before.
    Restored(BackendError),
    /// Some suspends could not be put back, so the task may run; `held` suspends remain.
    Partial
    {
        error: BackendError, held: u32
    },
}

/// How long detach waits for the private SIGSTOP to take effect before continuing the
/// released process.
const DETACH_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// How long detach keeps watching for a private SIGSTOP that took effect after its
/// first wait gave up and its SIGCONT was sent (#149). A stop landing in that time is
/// continued again; one never seen is reported.
const DETACH_STOP_RECHECK: Duration = Duration::from_secs(3);

/// How long `kill` waits for a launched process to be reaped.
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll `done` every millisecond until it holds or `timeout` passes; whether it held.
fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool
{
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

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
/// - `ThreadNotFound`, `NoThreadSelected`, `NoBreakpoint`, `BreakpointIdNotFound` ->
///   `NotFound`
/// - `ResourceExhausted` -> `Unsupported`
/// - `Os { code, .. }` keeps its code; an I/O error keeps its errno (or -1 without one)
/// - the engine's symbol and source errors, which no Mach helper returns, -> `Os` with
///   code -1, each named so a new category cannot fall into that arm unnoticed
///
/// Mach helpers build their `BackendError` with [`mach_failure`](crate::error::mach_failure)
/// while the `kern_return_t` is known, and it passes through unchanged.
pub(crate) fn backend_error(err: DebuggerError) -> BackendError
{
    let message = err.to_string();
    match err {
        DebuggerError::Backend(err) => err,
        DebuggerError::ProcessNotFound(_)
        | DebuggerError::ThreadNotFound(_)
        | DebuggerError::NoThreadSelected
        | DebuggerError::NoBreakpoint(_)
        | DebuggerError::BreakpointIdNotFound(_) => BackendError::NotFound(message),
        DebuggerError::PermissionDenied(detail) => BackendError::PermissionDenied(detail),
        DebuggerError::NotAttached
        | DebuggerError::NotStopped
        | DebuggerError::NotRunning
        | DebuggerError::AlreadyAttached => BackendError::InvalidState(message),
        DebuggerError::InvalidArgument(detail) | DebuggerError::ResourceExhausted(detail) => {
            BackendError::Unsupported(detail)
        }
        DebuggerError::Os { code, message } => BackendError::Os { code, message },
        DebuggerError::Io(io) => match io.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound(io.to_string()),
            std::io::ErrorKind::PermissionDenied => BackendError::PermissionDenied(io.to_string()),
            _ => BackendError::Os {
                code: io.raw_os_error().map_or(-1, i64::from),
                message: io.to_string(),
            },
        },
        DebuggerError::NoDebugInfo { .. }
        | DebuggerError::ImageMapping { .. }
        | DebuggerError::SymbolLoad { .. }
        | DebuggerError::SourceLookup(_) => BackendError::Os { code: -1, message },
    }
}

/// Map a failed `kern_return_t` of `what` to a [`BackendError`] that keeps the code.
fn mach_error(kr: libc::kern_return_t, what: &str) -> BackendError
{
    crate::error::mach_failure(what, kr)
}

/// Significant bits of an arm64 user pointer on macOS: xnu gives user processes a 47-bit
/// address space, so pointer-authentication bits are everything at and above bit 47.
pub const ARM64_ADDRESSABLE_BITS: u8 = 47;

/// What an `Undefined` return-address rule in arm64 call frame information means on
/// macOS: Apple's toolchain leaves that column undefined in the initial CIE row, so
/// before a prologue saves it the return address is still in `lr`.
pub const ARM64_UNDEFINED_RETURN_ADDRESS: UndefinedReturnAddress = UndefinedReturnAddress::LinkRegister;

/// Architecture knowledge for a macOS target of `architecture`, configured with the
/// platform's conventions ([`ARM64_ADDRESSABLE_BITS`],
/// [`ARM64_UNDEFINED_RETURN_ADDRESS`]). This is the [`Backend::arch`] of
/// [`MacOSBackend`].
///
/// # Errors
///
/// [`BackendError::Unsupported`] for an architecture other than arm64.
pub fn target_arch(architecture: Architecture) -> BackendResult<Arc<dyn Arch>>
{
    match architecture {
        Architecture::Arm64 => Ok(Arc::new(
            Arm64::with_addressable_bits(ARM64_ADDRESSABLE_BITS)?
                .with_undefined_return_address(ARM64_UNDEFINED_RETURN_ADDRESS),
        )),
        other => Err(BackendError::Unsupported(format!(
            "the macOS backend supports arm64 targets only, not {other}"
        ))),
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
/// and detaches from an attached process unless the session asks to keep it stopped.
/// In that case Ferros retains the task suspension and its installed exception ports
/// until the target dies. When the debugger process exits, its Mach rights (including
/// the exception port's receive right) are destroyed, and XNU sends SIGKILL to the
/// still-traced process as part of tracer exit: a held target does not outlive a
/// debugger, such as the CLI, that exits right after dropping its session
/// (`held_target_when_the_debugger_process_exits` in `ferros-tests/tests/native/lifecycle.rs`
/// observes the SIGKILL). If the exit watcher fails before the target dies, the
/// retained resources are not released.
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
    child_cleanup: launch::ChildCleanup,
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
    /// One-shot integration test fault for the next engine memory write.
    fail_memory_write: Arc<AtomicBool>,
    /// One-shot integration test fault: the next store of an engine write fails after
    /// the backend has planned it, before any byte reaches the target.
    fail_memory_store: Arc<AtomicBool>,
    /// One-shot integration test fault: the next interrupt of a running target neither
    /// suspends it nor queues `Interrupted`.
    drop_next_interrupt: Arc<AtomicBool>,
    /// One-shot integration test fault: the `task_suspend` in `hold_attached_on_drop` fails.
    fail_hold_suspend: Arc<AtomicBool>,
    /// One-shot integration test fault: the next `read_registers` asks `thread_get_state`
    /// for the state of `MACH_PORT_NULL`, so the real call fails.
    fail_register_read: Arc<AtomicBool>,
    /// One-shot integration test fault: the next launch fails to take the spawned
    /// child's task port. `0` unarmed, `-1` armed, otherwise the pid of the child that
    /// launch discarded.
    fail_launch_task: Arc<AtomicI32>,
    /// Sender side of the stop event channel (the exception loop holds a clone).
    events_tx: Option<mpsc::Sender<LoopEvent>>,
    /// Receiver side of the stop event channel, read by `wait`.
    events_rx: Option<mpsc::Receiver<LoopEvent>>,
    /// An event taken off the channel by `resume` while checking for pending events.
    peeked: Option<LoopEvent>,
    /// Cached memory pages for repeated reads; cleared whenever the target may run.
    memory_cache: MemoryCache,
    /// Mapped `LC_UUID` values by load address. Headers stay valid while the image stays
    /// mapped, so this survives stops and is cleared when the task goes away (#182).
    mapped_uuids: images::MappedUuidCache,
    /// Threads the last resume `thread_suspend`ed (action `Suspend`), each once; balanced
    /// with `thread_resume` when the next stop is delivered and on every teardown path.
    suspended_threads: Vec<thread_act_t>,
    /// Threads whose single-step bit the last resume set (action `Step`); the bit is
    /// cleared when the next stop is delivered and on teardown.
    stepping_threads: Vec<thread_act_t>,
}

impl MacOSBackend
{
    /// A cleanup handle for the next launch, shared with the backend's sole reaper.
    /// Not part of the supported API. Obtain it before moving the backend into a session.
    #[doc(hidden)]
    #[must_use]
    pub fn child_cleanup(&self) -> launch::ChildCleanup
    {
        self.child_cleanup.clone()
    }

    /// Create a new macOS backend with no process.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Session;
    /// use ferros_core::types::ProcessId;
    /// use ferros_macos::{MacOSBackend, MacOSRuntime, MachOFormat};
    ///
    /// let mut session = Session::new(Box::new(MacOSBackend::new()?), MachOFormat::shared())
    ///     .with_runtime(MacOSRuntime::shared());
    /// session.attach(ProcessId::from(12345))?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    ///
    /// ## Errors
    ///
    /// None at present; the `Result` leaves room for setup that can fail.
    pub fn new() -> Result<Self>
    {
        Ok(Self::empty())
    }

    fn empty() -> Self
    {
        Self {
            task: 0,
            threads: Vec::new(),
            pid: ProcessId(0),
            architecture: Architecture::current(),
            state: BackendState::NoProcess,
            launched: false,
            child_cleanup: launch::ChildCleanup::default(),
            traced: false,
            exit_watcher: None,
            exception_port: MACH_PORT_NULL,
            exception_thread: None,
            exception_commands: None,
            exception_state: Arc::new(Mutex::new(ExceptionSharedState::new())),
            saved_exception_ports: None,
            reply_faults: ReplyFaultInjector::default(),
            fail_memory_write: Arc::default(),
            fail_memory_store: Arc::default(),
            drop_next_interrupt: Arc::default(),
            fail_hold_suspend: Arc::default(),
            fail_register_read: Arc::default(),
            fail_launch_task: Arc::default(),
            events_tx: None,
            events_rx: None,
            peeked: None,
            memory_cache: MemoryCache::new(),
            mapped_uuids: images::MappedUuidCache::default(),
            suspended_threads: Vec::new(),
            stepping_threads: Vec::new(),
        }
    }

    /// Check if the debugger has permissions to attach to processes
    ///
    /// This function attempts to get a task port for the current process (self)
    /// to verify that debugging permissions are available. The send right the probe
    /// takes is released before returning.
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
        let mut task: mach_port_t = MACH_PORT_NULL;
        // SAFETY: `getpid` cannot fail; `task` is valid for writes.
        let result = unsafe { ffi::task_for_pid(mach_task_self(), libc::getpid(), &mut task) };

        match result {
            KERN_SUCCESS => {
                // SAFETY: `task` is the send right task_for_pid just gave us (a user
                // reference on our own task port), released once (issue #225).
                unsafe {
                    let _ = ffi::mach_port_deallocate(mach_task_self(), task);
                }
                Ok(true)
            }
            libc::KERN_PROTECTION_FAILURE => Ok(false),
            _ => Err(DebuggerError::from(MachError::from(result))),
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
    /// - `Backend` with the `kern_return_t` of a failed `thread_suspend()`
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
    /// - `Backend` with the `kern_return_t` of a failed `thread_resume()`
    pub fn resume_thread(&mut self, thread_id: ThreadId) -> Result<()>
    {
        threads::ThreadManager::resume_thread(&self.threads, thread_id)
    }

    /// Test hook: make the next exception reply report a failure.
    ///
    /// The reply is still sent so the thread is not stranded, but `wait` reports an
    /// injected `MACH_SEND_INVALID_DEST`. Client exception replies precede the stop
    /// event; drain the stop and then the error before resuming. An error from
    /// `wait` leaves the state unchanged.
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

    /// Test hook: arm a failure before the next engine memory write changes any bytes.
    #[doc(hidden)]
    pub fn memory_write_fault_injector(&self) -> impl FnOnce() + use<>
    {
        let fail = Arc::clone(&self.fail_memory_write);
        move || fail.store(true, Ordering::Relaxed)
    }

    /// Test hook: fail the store of the next engine memory write after the backend has
    /// planned it (including any birth-trap bookkeeping), before any byte is written.
    #[doc(hidden)]
    pub fn memory_store_fault_injector(&self) -> impl FnOnce() + use<>
    {
        let fail = Arc::clone(&self.fail_memory_store);
        move || fail.store(true, Ordering::Relaxed)
    }

    /// Test hook: make the next [`Backend::interrupt`] of a running target return `Ok`
    /// without suspending it or queueing `Interrupted`, as if the stop never arrived.
    ///
    /// A session dropped after arming it times out waiting for the stop, so
    /// `hold_attached_on_drop` runs with no suspension recorded. Tests only.
    #[doc(hidden)]
    pub fn interrupt_fault_injector(&self) -> impl FnOnce() + use<>
    {
        let drop_interrupt = Arc::clone(&self.drop_next_interrupt);
        move || drop_interrupt.store(true, Ordering::Relaxed)
    }

    /// Test hook: make the next `task_suspend` taken by `hold_attached_on_drop` fail with
    /// an injected `KERN_FAILURE` without suspending the task. Tests only.
    #[doc(hidden)]
    pub fn hold_suspend_fault_injector(&self) -> impl FnOnce() + use<>
    {
        let fail = Arc::clone(&self.fail_hold_suspend);
        move || fail.store(true, Ordering::Relaxed)
    }

    /// Test hook: make the next [`Backend::read_registers`] call `thread_get_state` on
    /// `MACH_PORT_NULL` instead of the thread's port, so the real call fails with its own
    /// `kern_return_t`. Tests only.
    #[doc(hidden)]
    pub fn register_read_fault_injector(&self) -> impl FnOnce() + use<>
    {
        let fail = Arc::clone(&self.fail_register_read);
        move || fail.store(true, Ordering::Relaxed)
    }

    /// Test hook: make the next [`Backend::launch`] fail with `PermissionDenied` after
    /// `posix_spawn` succeeds, as if `task_for_pid` were denied, so launch discards the
    /// child. The returned function reports that child's pid once the fault fired.
    /// Tests only.
    #[doc(hidden)]
    pub fn launch_task_fault_injector(&self) -> impl Fn() -> Option<ProcessId> + use<>
    {
        self.fail_launch_task.store(-1, Ordering::SeqCst);
        let fault = Arc::clone(&self.fail_launch_task);
        move || {
            u32::try_from(fault.load(Ordering::SeqCst))
                .ok()
                .filter(|&pid| pid != 0)
                .map(ProcessId)
        }
    }
}

mod backend;
mod handler;
mod ptrace;
mod state;
mod thread_actions;

#[cfg(test)]
mod tests
{
    use mach2::traps::mach_task_self;

    use super::MacOSBackend;
    use crate::diagnostics::send_right_count;

    /// `task_for_pid` on the calling process returns the name it already holds for its
    /// own task port, adding a user reference. Several probes must leave that count where
    /// it was. Other tests in this process may take and release the same right, so a leak
    /// is a growth by at least one reference per probe, never an exact match.
    #[test]
    fn permission_probe_does_not_leak_a_send_right()
    {
        const PROBES: u32 = 16;
        let backend = MacOSBackend::new().expect("backend");
        // SAFETY: `mach_task_self` takes no arguments and only returns the cached name of
        // this process's task port.
        let name = unsafe { mach_task_self() };
        let before = send_right_count(name).expect("refs before");
        for _ in 0..PROBES {
            assert!(
                backend.has_debugging_permissions().expect("probe"),
                "probing the calling task must succeed"
            );
        }
        let after = send_right_count(name).expect("refs after");
        assert!(
            after < before + PROBES,
            "{PROBES} probes left {} extra user references on the self task port ({before} before, {after} after)",
            after.saturating_sub(before)
        );
    }
}
