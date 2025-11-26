//! # macOS Task Management
//!
//! Mach task and thread management for debugging.
//!
//! On macOS, a **task** represents a process, and a **thread** represents
//! a thread within that process. To debug a process, we need to:
//!
//! 1. Get a Mach port to the task (using `task_for_pid()`)
//! 2. Enumerate threads in the task (using `task_threads()`)
//! 3. Read/write thread state (using `thread_get_state()` / `thread_set_state()`)
//!
//! ## Mach Ports
//!
//! Mach ports are communication channels to kernel objects (tasks, threads, etc.).
//! They're like file descriptors, but for kernel objects. When you get a task port,
//! you can use it to control that process.
//!
//! ## References
//!
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - [XNU Kernel Source](https://github.com/apple-oss-distributions/xnu) (for `task_for_pid` and `task_threads` implementation)

use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

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

use crate::breakpoints::{BreakpointEntry, BreakpointId, BreakpointInfo, BreakpointRequest, BreakpointStore};
use crate::debugger::Debugger;
use crate::error::{DebuggerError, Result};
use crate::events::{self, DebuggerEvent};
use crate::platform::macos::memory::{MemoryCache, get_memory_regions, write_memory};
#[cfg(target_arch = "aarch64")]
use crate::platform::macos::registers::{read_registers_arm64, write_registers_arm64};
#[cfg(target_arch = "x86_64")]
use crate::platform::macos::registers::{read_registers_x86_64, write_registers_x86_64};
use crate::platform::macos::{breakpoints, exception, ffi, launch, threads};
use crate::symbols::unwind::{MemoryAccess, StackUnwinder};
use crate::symbols::{ImageDescriptor, SymbolCache};
use crate::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, StackFrame, StopReason, ThreadId};

/// macOS debugger implementation using Mach APIs
///
/// This struct holds the state needed to debug a process on macOS.
///
/// ## Lifecycle
///
/// 1. Create: `MacOSDebugger::new()`
/// 2. Attach: `attach(pid)` - gets task port and main thread
/// 3. Use: `read_registers()`, etc.
/// 4. Detach: `detach()` - releases task port (or just drop the struct)
///
/// ## Thread Safety
///
/// Not thread-safe. Use from a single thread or wrap in `Mutex`.
pub struct MacOSDebugger
{
    /// Mach port to the target process (task)
    ///
    /// This is obtained from `task_for_pid()`. It's a handle that allows
    /// us to control the process. A value of 0 means we're not attached.
    ///
    /// See: [mach_port_t documentation](https://developer.apple.com/documentation/kernel/mach_port_t)
    task: mach_port_t,

    /// Cached thread ports for the target task.
    threads: Vec<thread_act_t>,

    /// Active thread used for register operations.
    current_thread: Option<thread_act_t>,

    /// Process ID of the target process
    ///
    /// Stored for reference and error messages. The actual debugging
    /// uses the `task` port, not the PID.
    pid: ProcessId,
    /// Architecture metadata.
    architecture: Architecture,
    /// Whether we're currently attached to a process.
    attached: bool,
    /// Mach exception port used to receive stop notifications.
    exception_port: mach_port_t,
    /// Thread running the Mach exception handling loop.
    exception_thread: Option<thread::JoinHandle<()>>,
    /// Channel used to signal the exception loop (resume/shutdown).
    exception_resume_tx: Option<mpsc::Sender<exception::ExceptionLoopCommand>>,
    /// Shared exception state observed by both the handler loop and debugger.
    exception_state: Arc<Mutex<exception::ExceptionSharedState>>,
    /// Breakpoint store shared with the exception handler.
    breakpoints: Arc<Mutex<BreakpointStore>>,
    /// Event channel sender for higher-level consumers.
    event_tx: events::DebuggerEventSender,
    /// Event channel receiver handed out to frontends.
    event_rx: Option<events::DebuggerEventReceiver>,
    /// Whether stdout/stderr should be captured for launched processes.
    capture_output: bool,
    /// Read end of the stdout pipe for the most recently launched process.
    stdout_pipe: Option<File>,
    /// Read end of the stderr pipe for the most recently launched process.
    stderr_pipe: Option<File>,
    /// Symbol cache for DWARF and symbol resolution.
    symbol_cache: SymbolCache,
    /// Cached memory pages for repeated reads.
    memory_cache: MemoryCache,
}

// Trait implementations for modular operations
impl breakpoints::BreakpointOperations for MacOSDebugger
{
    fn read_memory(&self, addr: Address, len: usize) -> Result<Vec<u8>>
    {
        crate::platform::macos::memory::read_memory(self.task, addr, len)
    }

    fn write_memory(&mut self, addr: Address, data: &[u8]) -> Result<usize>
    {
        write_memory(self.task, addr, data)
    }

    fn thread_ports(&self) -> &[thread_act_t]
    {
        &self.threads
    }

    fn architecture(&self) -> Architecture
    {
        self.architecture
    }

    fn ensure_attached(&self) -> Result<()>
    {
        if !self.attached || self.task == 0 {
            return Err(DebuggerError::NotAttached);
        }
        Ok(())
    }
}

impl threads::ThreadOperations for MacOSDebugger
{
    fn task_port(&self) -> libc::mach_port_t
    {
        self.task
    }

    fn thread_ports_mut(&mut self) -> &mut Vec<thread_act_t>
    {
        &mut self.threads
    }

    fn thread_ports(&self) -> &[thread_act_t]
    {
        &self.threads
    }

    fn current_thread(&self) -> Option<thread_act_t>
    {
        self.current_thread
    }

    fn set_current_thread(&mut self, thread: Option<thread_act_t>)
    {
        self.current_thread = thread;
    }

    fn pid(&self) -> u32
    {
        self.pid.0
    }
}

impl launch::LaunchOperations for MacOSDebugger
{
    fn capture_output(&self) -> bool
    {
        self.capture_output
    }

    fn set_stdout_pipe(&mut self, fd: RawFd)
    {
        use std::fs::File;
        unsafe {
            self.stdout_pipe = Some(File::from_raw_fd(fd));
        }
    }

    fn set_stderr_pipe(&mut self, fd: RawFd)
    {
        use std::fs::File;
        unsafe {
            self.stderr_pipe = Some(File::from_raw_fd(fd));
        }
    }
}

impl MacOSDebugger
{
    /// Create a new macOS debugger instance
    ///
    /// This doesn't attach to any process yet - it just creates an empty
    /// debugger ready to attach. Call `attach()` to actually connect to a process.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ProcessId;
    ///
    /// let mut debugger = MacOSDebugger::new()?;
    /// debugger.attach(ProcessId::from(12345))?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn new() -> Result<Self>
    {
        let (event_tx, event_rx) = events::event_channel();

        Ok(Self {
            task: 0,
            threads: Vec::new(),
            current_thread: None,
            pid: ProcessId(0),
            architecture: Architecture::current(),
            attached: false,
            exception_port: MACH_PORT_NULL,
            exception_thread: None,
            exception_resume_tx: None,
            exception_state: Arc::new(Mutex::new(exception::ExceptionSharedState::new())),
            breakpoints: Arc::new(Mutex::new(BreakpointStore::new())),
            event_tx,
            event_rx: Some(event_rx),
            capture_output: false,
            stdout_pipe: None,
            stderr_pipe: None,
            symbol_cache: SymbolCache::new(),
            memory_cache: MemoryCache::new(),
        })
    }

    fn publish_stop_event(&self, reason: StopReason, thread: Option<thread_act_t>)
    {
        let thread_id = thread.map(|port| ThreadId::from(port as u64));
        if let Err(err) = self.event_tx.send(DebuggerEvent::TargetStopped {
            reason,
            thread: thread_id,
        }) {
            tracing::warn!("Failed to dispatch stop event: {err}");
        }
    }

    fn publish_resumed_event(&self)
    {
        if let Err(err) = self.event_tx.send(DebuggerEvent::TargetResumed) {
            tracing::warn!("Failed to dispatch resume event: {err}");
        }
    }

    /// Check if the debugger has permissions to attach to processes
    ///
    /// This function attempts to get a task port for the current process (self)
    /// to verify that debugging permissions are available. This is useful for
    /// checking permissions before attempting to attach to other processes.
    ///
    /// ## Returns
    ///
    /// - `Ok(true)`: Debugging permissions are available
    /// - `Ok(false)`: Debugging permissions are not available (need sudo or entitlements)
    /// - `Err(...)`: Error occurred while checking permissions
    ///
    /// ## Note
    ///
    /// This function checks permissions by attempting `task_for_pid()` on the current
    /// process. Even if this succeeds, attaching to other processes may still fail
    /// due to System Integrity Protection (SIP) restrictions.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::platform::macos::MacOSDebugger;
    ///
    /// let debugger = MacOSDebugger::new()?;
    /// if debugger.has_debugging_permissions()? {
    ///     println!("✅ Debugging permissions available");
    /// } else {
    ///     println!("❌ Need debugging permissions");
    ///     println!("   Quick fix: Run with sudo");
    ///     println!("   Or use launch() to spawn processes (doesn't need permissions)");
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn has_debugging_permissions(&self) -> Result<bool>
    {
        unsafe {
            let current_pid = libc::getpid();
            let mut task: mach_port_t = 0;
            let result = ffi::task_for_pid(mach_task_self(), current_pid, &mut task);

            match result {
                KERN_SUCCESS => Ok(true),
                libc::KERN_PROTECTION_FAILURE => Ok(false),
                _ => Err(DebuggerError::MachError(result.into())),
            }
        }
    }

    /// Common attachment logic shared by `attach()` and `launch()`
    ///
    /// This obtains the Mach task port and thread list for the target PID but
    /// does not modify the task's execution state. Callers decide whether to
    /// suspend or resume after attaching.
    ///
    /// ## Mach APIs Used
    ///
    /// - **task_for_pid()**: Obtains a Mach task port for the process
    /// - **task_threads()**: Enumerates all threads in the task
    ///
    /// ## Permissions
    ///
    /// Requires debugging permissions (sudo or entitlements) to call `task_for_pid()`.
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

                    if process_exists {
                        return Err(DebuggerError::PermissionDenied(format!(
                            "task_for_pid() failed with KERN_FAILURE, but process {} exists. This means insufficient \
                             permissions.\n\nQuick fix: Run with sudo:\n  sudo ferros attach {}\n\nAlternatively, use \
                             launch() to spawn processes under debugger control, which doesn't require special permissions.",
                            pid.0, pid.0
                        )));
                    }
                }

                return Err(DebuggerError::MachError(result.into()));
            }

            let mut threads: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;

            let result = task_threads(task, &mut threads, &mut thread_count);
            if result != KERN_SUCCESS || thread_count == 0 {
                threads::ThreadManager::deallocate_threads_array(threads, thread_count);
                return Err(DebuggerError::AttachFailed(format!("Failed to get threads: {}", result)));
            }

            let slice = std::slice::from_raw_parts(threads, thread_count as usize);
            self.task = task;
            self.pid = pid;
            self.threads = slice.to_vec();
            threads::ThreadManager::deallocate_threads_array(threads, thread_count);
            self.current_thread = self.threads.first().copied();
            self.attached = true;
        }

        {
            let mut shared = self.exception_state.lock().unwrap();
            shared.stopped = false;
            shared.stop_reason = StopReason::Running;
            shared.pending_thread = None;
            self.memory_cache.clear();
        }

        self.start_exception_handler()?;

        Ok(())
    }

    /// Ensure that the debugger is attached to a process
    ///
    /// This is an internal helper method that checks if the debugger is currently
    /// attached. It returns an error if not attached, allowing methods to fail early
    /// with a clear error message.
    ///
    /// ## Errors
    ///
    /// Returns `NotAttached` if:
    /// - The debugger was never attached (`attached == false`)
    /// - The task port is invalid (`task == 0`)
    ///
    /// ## Example
    ///
    /// This is an internal helper method. Public methods like `read_registers()`
    /// call this internally to ensure the debugger is attached before proceeding.
    fn ensure_attached(&self) -> Result<()>
    {
        if !self.attached || self.task == 0 {
            return Err(DebuggerError::NotAttached);
        }
        Ok(())
    }

    /// Get the Mach thread port for the currently active thread
    ///
    /// This is an internal helper method that returns the `thread_act_t` (Mach thread port)
    /// for the active thread. It's used by register operations to know which thread
    /// to read/write registers from.
    ///
    /// ## Errors
    ///
    /// Returns `InvalidArgument` if no active thread has been selected.
    ///
    /// ## See Also
    ///
    /// - `active_thread()`: Public API to get the active thread ID
    /// - `set_active_thread()`: Public API to set the active thread
    fn active_thread_port(&self) -> Result<thread_act_t>
    {
        threads::ThreadManager::active_thread_port(self)
    }

    fn thread_port_for_id(&self, thread: ThreadId) -> Result<thread_act_t>
    {
        threads::ThreadManager::thread_port_for_id(self, thread)
    }

    // Internal breakpoint methods - these are wrappers around BreakpointManager
    // methods. They're kept for potential future use or internal consistency.
    #[allow(dead_code)]
    fn install_software_breakpoint(&mut self, address: Address) -> Result<BreakpointId>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::install_software_breakpoint(self, &breakpoints, address)
    }

    #[allow(dead_code)]
    fn install_hardware_breakpoint(&mut self, address: Address) -> Result<BreakpointId>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::install_hardware_breakpoint(self, &breakpoints, address)
    }

    #[allow(dead_code)]
    fn restore_software_breakpoint(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        breakpoints::BreakpointManager::restore_software_breakpoint(self, entry)
    }

    #[allow(dead_code)]
    fn remove_hardware_breakpoint(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        breakpoints::BreakpointManager::remove_hardware_breakpoint(self, entry)
    }

    fn restore_all_breakpoints(&mut self)
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::restore_all_breakpoints(self, &breakpoints);
    }

    fn read_registers_from_port(&self, thread: thread_act_t) -> Result<Registers>
    {
        match self.architecture {
            Architecture::Arm64 => {
                #[cfg(target_arch = "aarch64")]
                {
                    read_registers_arm64(thread)
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
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
                    Err(DebuggerError::InvalidArgument(
                        "x86_64 register access not supported on this build".to_string(),
                    ))
                }
            }
            Architecture::Unknown(label) => {
                Err(DebuggerError::InvalidArgument(format!("Unsupported architecture: {label}")))
            }
        }
    }

    fn write_registers_to_port(&self, thread: thread_act_t, regs: &Registers) -> Result<()>
    {
        match self.architecture {
            Architecture::Arm64 => {
                #[cfg(target_arch = "aarch64")]
                {
                    write_registers_arm64(thread, regs)
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
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
                    Err(DebuggerError::InvalidArgument(
                        "x86_64 register access not supported on this build".to_string(),
                    ))
                }
            }
            Architecture::Unknown(label) => {
                Err(DebuggerError::InvalidArgument(format!("Unsupported architecture: {label}")))
            }
        }
    }

    /// Set the active thread using a Mach thread port
    ///
    /// This is an internal helper method that sets the active thread using a raw
    /// `thread_act_t` (Mach thread port). It validates that the thread belongs
    /// to the current process before setting it as active.
    ///
    /// ## Parameters
    ///
    /// - `port`: The Mach thread port (`thread_act_t`) to make active
    ///
    /// ## Errors
    ///
    /// Returns `InvalidArgument` if the thread port is not in the current thread list.
    ///
    /// ## See Also
    ///
    /// - `set_active_thread()`: Public API that takes a `ThreadId` instead
    fn set_active_thread_by_port(&mut self, port: thread_act_t) -> Result<()>
    {
        threads::ThreadManager::set_active_thread_by_port(self, port)
    }

    fn refresh_thread_list(&mut self) -> Result<()>
    {
        self.ensure_attached()?;
        threads::ThreadManager::refresh_thread_list(self)
    }

    /// Attempt to continue execution if we're currently stopped inside the Mach exception loop.
    ///
    /// Returns `Ok(true)` if a pending exception reply was sent, or `Ok(false)` if
    /// the normal `task_resume` path should be used instead.
    fn try_resume_pending_exception(&mut self) -> Result<bool>
    {
        let has_pending = self.exception_state.lock().unwrap().pending_thread.is_some();
        if !has_pending {
            return Ok(false);
        }

        let sender = self
            .exception_resume_tx
            .as_ref()
            .ok_or_else(|| DebuggerError::ResumeFailed("exception handler not running".to_string()))?;

        sender
            .send(exception::ExceptionLoopCommand::Continue)
            .map_err(|_| DebuggerError::ResumeFailed("failed to signal exception handler".to_string()))?;

        Ok(true)
    }

    /// Start the Mach exception handler thread.
    ///
    /// This creates a Mach receive port, registers it with `task_set_exception_ports()`,
    /// and spawns a background thread to handle exceptions (breakpoints, signals, etc.).
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
    /// ## Thread Safety
    ///
    /// The exception handler runs in a separate thread and communicates via channels.
    /// See `exception::run_exception_loop()` for the handler implementation.
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
    /// - [mach_port_allocate(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_port_allocate/)
    fn start_exception_handler(&mut self) -> Result<()>
    {
        #[cfg(target_os = "macos")]
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
                    return Err(DebuggerError::MachError(kr.into()));
                }

                kr = mach_port_insert_right(self_task, port, port, MACH_MSG_TYPE_MAKE_SEND);
                if kr != KERN_SUCCESS {
                    let _ = mach_port_destroy(self_task, port);
                    return Err(DebuggerError::MachError(kr.into()));
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
                    return Err(DebuggerError::MachError(kr.into()));
                }

                let (tx, rx) = mpsc::channel();
                let shared_state = Arc::clone(&self.exception_state);
                let breakpoints = self.breakpoints.clone();
                let architecture = self.architecture;
                let event_tx = self.event_tx.clone();
                info!("Spawning Mach exception handler thread");
                let handle = thread::Builder::new()
                    .name("ferros-mac-exc".to_string())
                    .spawn(move || {
                        exception::run_exception_loop(port, rx, shared_state, architecture, event_tx, breakpoints)
                    })
                    .map_err(|e| {
                        let _ = mach_port_destroy(self_task, port);
                        DebuggerError::AttachFailed(format!("Failed to spawn exception handler: {e}"))
                    })?;

                self.exception_port = port;
                self.exception_thread = Some(handle);
                self.exception_resume_tx = Some(tx);
            }
        }

        Ok(())
    }

    fn stop_exception_handler(&mut self)
    {
        #[cfg(target_os = "macos")]
        {
            if let Some(tx) = self.exception_resume_tx.take() {
                let _ = tx.send(exception::ExceptionLoopCommand::Shutdown);
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

            let mut shared = self.exception_state.lock().unwrap();
            shared.pending_thread = None;
            shared.stopped = false;
            shared.stop_reason = StopReason::Running;
        }
    }
}

impl Debugger for MacOSDebugger
{
    fn set_capture_process_output(&mut self, capture: bool)
    {
        self.capture_output = capture;
        if !capture {
            self.stdout_pipe = None;
            self.stderr_pipe = None;
        }
    }

    fn take_process_stdout(&mut self) -> Option<File>
    {
        self.stdout_pipe.take()
    }

    fn take_process_stderr(&mut self) -> Option<File>
    {
        self.stderr_pipe.take()
    }

    fn take_event_receiver(&mut self) -> Option<events::DebuggerEventReceiver>
    {
        self.event_rx.take()
    }

    fn add_breakpoint(&mut self, request: BreakpointRequest) -> Result<BreakpointId>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::add_breakpoint(self, &breakpoints, request)
    }

    fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::remove_breakpoint(self, &breakpoints, id)
    }

    fn enable_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::enable_breakpoint(self, &breakpoints, id)
    }

    fn disable_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::disable_breakpoint(self, &breakpoints, id)
    }

    fn toggle_breakpoint(&mut self, id: BreakpointId) -> Result<bool>
    {
        let breakpoints = self.breakpoints.clone();
        breakpoints::BreakpointManager::toggle_breakpoint(self, &breakpoints, id)
    }

    fn breakpoint_info(&self, id: BreakpointId) -> Result<BreakpointInfo>
    {
        breakpoints::BreakpointManager::breakpoint_info(&self.breakpoints, id)
    }

    fn breakpoints(&self) -> Vec<BreakpointInfo>
    {
        breakpoints::BreakpointManager::breakpoints(&self.breakpoints)
    }

    fn read_registers_for(&self, thread: ThreadId) -> Result<Registers>
    {
        let port = self.thread_port_for_id(thread)?;
        self.read_registers_from_port(port)
    }

    fn write_registers_for(&mut self, thread: ThreadId, regs: &Registers) -> Result<()>
    {
        let port = self.thread_port_for_id(thread)?;
        self.write_registers_to_port(port, regs)
    }

    fn stack_trace(&mut self, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        self.ensure_attached()?;
        let thread = self.active_thread_port()?;
        let thread_id = ThreadId::from(thread as u64);
        let regs = self.read_registers_from_port(thread)?;

        // Load images into symbol cache if not already loaded
        let regions = get_memory_regions(self.task)?;
        
        // First, try to load the main executable explicitly
        // Find the region containing the PC (which should be in the main executable)
        let exec_path = Self::get_executable_path(self.pid);
        if let Some(exec_path) = &exec_path {
            let pc_addr = regs.pc.value();
            // Find the region containing the PC
            if let Some(pc_region) = regions.iter().find(|r| {
                r.start.value() <= pc_addr && pc_addr < r.end.value()
            }) {
                // Try loading the executable at this region's start address
                let desc = ImageDescriptor {
                    path: exec_path.clone(),
                    load_address: pc_region.start.value(),
                };
                let _ = self.symbol_cache.load_image(desc);
            }
        }

        // Load all other images (shared libraries and named regions)
        for region in regions {
            if let Some(name) = &region.name
                && name.starts_with('/')
                && (name.ends_with(".dylib") || name.ends_with(".so") || !name.contains('['))
            {
                // Skip if this is the main executable (we already loaded it above)
                if let Some(ref exec_path) = exec_path {
                    if name == exec_path.to_str().unwrap_or("") {
                        continue;
                    }
                }
                let desc = ImageDescriptor {
                    path: std::path::PathBuf::from(name),
                    load_address: region.start.value(),
                };
                let _ = self.symbol_cache.load_image(desc);
            }
        }

        // Implement MemoryAccess for MacOSDebugger
        struct MacOSMemoryAccess<'a>
        {
            task: mach_port_t,
            cache: &'a MemoryCache,
        }

        impl<'a> MemoryAccess for MacOSMemoryAccess<'a>
        {
            fn read_u64(&self, address: Address) -> Result<u64>
            {
                self.cache.read_u64(self.task, address)
            }
        }

        let memory = MacOSMemoryAccess {
            task: self.task,
            cache: &self.memory_cache,
        };

        let unwinder = StackUnwinder::new(self.architecture, &self.symbol_cache, &memory);
        unwinder.unwind(thread_id, &regs, max_frames)
    }

    /// Launch a new process under debugger control using posix_spawn
    ///
    /// This function:
    /// 1. Uses `posix_spawn()` with `POSIX_SPAWN_START_SUSPENDED` to spawn the process
    /// 2. Immediately attaches to it using `attach()` to get the task port
    /// 3. The process remains suspended, ready for debugging
    ///
    /// ## Advantages
    ///
    /// - Process starts suspended, so you can set breakpoints before execution
    /// - More reliable than attaching to already-running processes
    /// - Better control over the process lifecycle
    ///
    /// ## Note on Permissions
    ///
    /// Even when launching processes, `attach()` is still called internally to get
    /// the task port, which requires debugging permissions. However, launching
    /// processes you own may have different permission requirements than attaching
    /// to arbitrary processes. If you encounter permission errors, try:
    /// - Running with `sudo`
    /// - Code signing with the `com.apple.security.cs.debugger` entitlement
    ///
    /// ## Platform Requirements
    ///
    /// - **macOS 10.5+**: `POSIX_SPAWN_START_SUSPENDED` flag is available
    /// - **Architecture**: Supports both ARM64 (Apple Silicon) and x86_64 (Intel)
    ///
    /// ## Parameters
    ///
    /// - `program`: Path to the executable to launch (must be absolute or in PATH)
    /// - `args`: Command-line arguments (first argument should typically be the program name)
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Invalid program path or empty arguments
    /// - `AttachFailed`: Failed to spawn process or attach to it
    /// - `Io`: I/O error (e.g., file not found, permission denied)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    ///
    /// let mut debugger = MacOSDebugger::new()?;
    /// debugger.launch("/usr/bin/echo", &["echo", "Hello, world!"])?;
    /// // Process is now suspended and ready for debugging
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn launch(&mut self, program: &str, args: &[&str]) -> Result<ProcessId>
    {
        use tracing::{debug, info};

        info!("Launching process: {} with args: {:?}", program, args);
        let pid = launch::LaunchManager::launch(self, program, args)?;
        debug!("Attaching to spawned process");
        // Attach to the spawned process
        let process_id = ProcessId::from(pid as u32);
        self.attach_task(process_id)?;
        {
            let mut shared = self.exception_state.lock().unwrap();
            shared.stopped = true;
            shared.stop_reason = StopReason::Suspended;
        }
        info!("Successfully launched and attached to process {}", pid);
        Ok(process_id)
    }

    /// Attach to a running process using Mach APIs
    ///
    /// This function:
    /// 1. Calls `task_for_pid()` to get a Mach port to the process
    /// 2. Calls `task_threads()` to enumerate threads
    /// 3. Stores the main thread for later use
    ///
    /// ## Mach API: task_for_pid()
    ///
    /// ```c
    /// kern_return_t task_for_pid(
    ///     mach_port_t target_task,  // Our own task port (mach_task_self())
    ///     int pid,                  // PID of target process
    ///     mach_port_t *task         // Output: task port for target process
    /// );
    /// ```
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// **Security**: Requires special permissions (sudo or debugging entitlements).
    ///
    /// See: [XNU Kernel Source](https://github.com/apple-oss-distributions/xnu) for `task_for_pid` implementation
    ///
    /// ## Mach API: task_threads()
    ///
    /// ```c
    /// kern_return_t task_threads(
    ///     task_t target_task,           // Task port from task_for_pid()
    ///     thread_act_array_t *act_list, // Output: array of thread ports
    ///     mach_msg_type_number_t *count // Output: number of threads
    /// );
    /// ```
    ///
    /// **Returns**: Array of thread ports. We use the first one as the main thread.
    ///
    /// See: [XNU Kernel Source](https://github.com/apple-oss-distributions/xnu) for `task_threads` implementation
    ///
    /// ## Errors
    ///
    /// - `MachError::ProtectionFailure`: Need sudo or entitlements
    /// - `MachError::InvalidArgument`: Invalid PID
    /// - `MachError::ProcessNotFound`: Process doesn't exist
    /// - `AttachFailed`: Failed to get threads
    fn attach(&mut self, pid: ProcessId) -> Result<()>
    {
        self.stdout_pipe = None;
        self.stderr_pipe = None;
        self.attach_task(pid)?;
        // Suspend immediately so the debugger has control.
        self.suspend()?;
        Ok(())
    }

    /// Detach from the process
    ///
    /// This function properly releases the Mach ports obtained during attachment.
    /// It calls `mach_port_deallocate()` to release:
    /// - The task port (obtained from `task_for_pid()`)
    /// - All thread ports (obtained from `task_threads()`)
    ///
    /// ## Mach API: mach_port_deallocate()
    ///
    /// ```c
    /// kern_return_t mach_port_deallocate(
    ///     mach_port_t target_task,  // Task port that owns the port
    ///     mach_port_t name          // Port to deallocate
    /// );
    /// ```
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [mach_port_deallocate documentation](https://developer.apple.com/documentation/kernel/1578777-mach_port_deallocate/)
    ///
    /// ## Implementation Notes
    ///
    /// - Deallocates the task port first, then all thread ports
    /// - Errors during deallocation are logged but don't prevent cleanup
    /// - After detaching, the debugger is in an uninitialized state
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process (no-op)
    fn detach(&mut self) -> Result<()>
    {
        use tracing::{debug, info};

        if !self.attached {
            debug!("Detach called but not attached, no-op");
            return Ok(());
        }

        self.stop_exception_handler();
        self.restore_all_breakpoints();

        let pid = self.pid.0;
        info!("Detaching from process {}", pid);
        debug!("Deallocating Mach ports for process {}", pid);

        unsafe {
            // Deallocate all thread ports first
            debug!("Deallocating {} thread ports", self.threads.len());
            for thread in &self.threads {
                let _ = ffi::mach_port_deallocate(mach_task_self(), *thread);
            }

            // Deallocate the task port
            if self.task != 0 {
                debug!("Deallocating task port");
                let _ = ffi::mach_port_deallocate(mach_task_self(), self.task);
            }
        }

        // Clear all state
        self.task = 0;
        self.threads.clear();
        self.current_thread = None;
        self.pid = ProcessId(0);
        self.attached = false;
        {
            let mut shared = self.exception_state.lock().unwrap();
            shared.stopped = false;
            shared.stop_reason = StopReason::Running;
            shared.pending_thread = None;
        }

        info!("Successfully detached from process {}", pid);
        Ok(())
    }

    /// Read registers from the attached process
    ///
    /// Delegates to platform-specific register reading functions based on
    /// the CPU architecture (ARM64 vs x86-64).
    ///
    /// ## Architecture Detection
    ///
    /// We use `#[cfg(target_arch = "...")]` to compile different code
    /// for different architectures:
    ///
    /// - `aarch64`: Apple Silicon (M1, M2, M3, M4, etc.)
    /// - `x86_64`: Intel Macs
    ///
    /// This is done at compile time, so there's no runtime overhead.
    fn read_registers(&self) -> Result<Registers>
    {
        self.ensure_attached()?;
        let thread = self.active_thread_port()?;
        self.read_registers_from_port(thread)
    }

    fn write_registers(&mut self, regs: &Registers) -> Result<()>
    {
        self.ensure_attached()?;
        let thread = self.active_thread_port()?;
        self.write_registers_to_port(thread, regs)
    }

    /// Read memory from the target process
    ///
    /// Uses `vm_read()` to read memory from the Mach task.
    fn read_memory(&self, addr: Address, len: usize) -> Result<Vec<u8>>
    {
        self.ensure_attached()?;
        self.memory_cache.read(self.task, addr, len)
    }

    /// Write memory to the target process
    ///
    /// Uses `vm_write()` to write memory to the Mach task.
    fn write_memory(&mut self, addr: Address, data: &[u8]) -> Result<usize>
    {
        self.ensure_attached()?;
        let written = write_memory(self.task, addr, data)?;
        if written > 0 {
            self.memory_cache.invalidate_range(addr, written);
        }
        Ok(written)
    }

    /// Get memory regions for the attached process
    ///
    /// Uses `vm_region()` to enumerate memory regions in the Mach task.
    fn get_memory_regions(&self) -> Result<Vec<MemoryRegion>>
    {
        self.ensure_attached()?;
        get_memory_regions(self.task)
    }

    fn architecture(&self) -> Architecture
    {
        self.architecture
    }

    fn is_attached(&self) -> bool
    {
        self.attached
    }

    fn is_stopped(&self) -> bool
    {
        self.exception_state.lock().unwrap().stopped
    }

    fn stop_reason(&self) -> StopReason
    {
        self.exception_state.lock().unwrap().stop_reason
    }

    /// Suspend execution of the target process using Mach APIs
    ///
    /// Calls `task_suspend()` to suspend the Mach task. This stops all threads
    /// in the process, allowing safe inspection of registers and memory.
    ///
    /// ## Mach API: task_suspend()
    ///
    /// ```c
    /// kern_return_t task_suspend(task_t target_task);
    /// ```
    ///
    /// **Parameters**:
    /// - `target_task`: Task port from `task_for_pid()`
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [task_suspend documentation](https://developer.apple.com/documentation/kernel/1402800-task_suspend)
    ///
    /// ## Implementation Notes
    ///
    /// - If the process is already stopped, this is a no-op
    /// - Updates internal state (`stopped`, `stop_reason`) after successful suspension
    /// - All threads in the task are suspended atomically
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `SuspendFailed`: `task_suspend()` failed
    fn suspend(&mut self) -> Result<()>
    {
        use tracing::{debug, info};

        self.ensure_attached()?;
        if self.is_stopped() {
            debug!("Process {} already suspended", self.pid.0);
            return Ok(());
        }

        info!("Suspending process {}", self.pid.0);
        debug!("Calling task_suspend for process {}", self.pid.0);

        unsafe {
            let result = task_suspend(self.task);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::SuspendFailed(format!("task_suspend failed: {}", result)));
            }
        }

        {
            let mut shared = self.exception_state.lock().unwrap();
            shared.stopped = true;
            shared.stop_reason = StopReason::Suspended;
            shared.pending_thread = None;
        }
        self.publish_stop_event(StopReason::Suspended, None);
        info!("Successfully suspended process {}", self.pid.0);
        Ok(())
    }

    /// Resume execution of the target process using Mach APIs
    ///
    /// Calls `task_resume()` to resume the Mach task. This resumes all threads
    /// in the process, allowing it to continue execution.
    ///
    /// ## Mach API: task_resume()
    ///
    /// ```c
    /// kern_return_t task_resume(task_t target_task);
    /// ```
    ///
    /// **Parameters**:
    /// - `target_task`: Task port from `task_for_pid()`
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [task_resume documentation](https://developer.apple.com/documentation/kernel/1402801-task_resume)
    ///
    /// ## Implementation Notes
    ///
    /// - If the process is already running, this is a no-op
    /// - Updates internal state (`stopped`, `stop_reason`) after successful resume
    /// - All threads in the task are resumed atomically
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `ResumeFailed`: `task_resume()` failed
    fn resume(&mut self) -> Result<()>
    {
        use tracing::{debug, info};

        self.ensure_attached()?;
        if self.try_resume_pending_exception()? {
            info!("Continuing from Mach exception for process {}", self.pid.0);
            return Ok(());
        }
        if !self.is_stopped() {
            debug!("Process {} already running", self.pid.0);
            return Ok(());
        }

        info!("Resuming process {}", self.pid.0);
        debug!("Calling task_resume for process {}", self.pid.0);

        unsafe {
            let result = task_resume(self.task);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::ResumeFailed(format!("task_resume failed: {}", result)));
            }
        }

        {
            let mut shared = self.exception_state.lock().unwrap();
            shared.stopped = false;
            shared.stop_reason = StopReason::Running;
            shared.pending_thread = None;
        }
        self.publish_resumed_event();
        info!("Successfully resumed process {}", self.pid.0);
        Ok(())
    }

    /// List all threads in the target process
    ///
    /// Returns the cached thread list as `ThreadId` values. The list is maintained
    /// internally and updated when `refresh_threads()` is called.
    ///
    /// ## Thread List Caching
    ///
    /// The thread list is cached for performance. It's updated:
    /// - When `attach()` is called (initial enumeration)
    /// - When `refresh_threads()` is called (manual refresh)
    ///
    /// If threads are created or destroyed, call `refresh_threads()` to update the list.
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    fn threads(&self) -> Result<Vec<ThreadId>>
    {
        self.ensure_attached()?;
        Ok(self.threads.iter().copied().map(|t| ThreadId::from(t as u64)).collect())
    }

    /// Get the currently active thread
    ///
    /// Returns `Some(thread_id)` if an active thread has been selected, or `None`
    /// if no thread is active. The active thread is used for register operations.
    ///
    /// ## Default Behavior
    ///
    /// When attaching to a process, the first thread (typically the main thread)
    /// is automatically selected as the active thread.
    fn active_thread(&self) -> Option<ThreadId>
    {
        self.current_thread.map(|t| ThreadId::from(t as u64))
    }

    /// Set the active thread for register operations
    ///
    /// Sets the active thread by converting the `ThreadId` to a Mach thread port
    /// and calling `set_active_thread_by_port()`.
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: The thread ID is not valid (not in the thread list)
    fn set_active_thread(&mut self, thread: ThreadId) -> Result<()>
    {
        self.set_active_thread_by_port(thread.raw() as thread_act_t)
    }

    /// Refresh the thread list from the operating system
    ///
    /// Updates the cached thread list by calling `refresh_thread_list()`, which
    /// queries the operating system for the current set of threads.
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `AttachFailed`: `task_threads()` failed
    fn refresh_threads(&mut self) -> Result<()>
    {
        self.refresh_thread_list()
    }
}

impl Drop for MacOSDebugger
{
    fn drop(&mut self)
    {
        self.restore_all_breakpoints();
        self.stop_exception_handler();
    }
}

impl MacOSDebugger
{
    /// Suspend a specific thread
    ///
    /// This function suspends execution of a single thread within the target task.
    /// Unlike `suspend()`, which suspends all threads, this allows fine-grained control
    /// over individual threads.
    ///
    /// ## Mach API: thread_suspend()
    ///
    /// ```c
    /// kern_return_t thread_suspend(thread_act_t target_act);
    /// ```
    ///
    /// **Parameters**:
    /// - `target_act`: Thread port (from `task_threads()`) to suspend
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [thread_suspend documentation](https://developer.apple.com/documentation/kernel/1418833-thread_suspend/)
    ///
    /// ## Architecture Notes
    ///
    /// - **ARM64**: `thread_suspend()` is the preferred method for per-thread control
    /// - **Intel**: You may need to use `thread_set_state()` with `X86_THREAD_STATE64` flavor
    ///   to coordinate per-thread operations (not yet implemented)
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Thread ID is not valid (not in the thread list)
    /// - `SuspendFailed`: `thread_suspend()` failed
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ThreadId;
    ///
    /// # let mut debugger = MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let threads = debugger.threads()?;
    /// if let Some(thread) = threads.first() {
    ///     debugger.suspend_thread(*thread)?;
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn suspend_thread(&mut self, thread_id: ThreadId) -> Result<()>
    {
        self.ensure_attached()?;
        threads::ThreadManager::suspend_thread(self, thread_id)
    }

    /// Resume a specific thread
    ///
    /// This function resumes execution of a single thread within the target task.
    /// The thread will continue from where it was suspended.
    ///
    /// ## Mach API: thread_resume()
    ///
    /// ```c
    /// kern_return_t thread_resume(thread_act_t target_act);
    /// ```
    ///
    /// **Parameters**:
    /// - `target_act`: Thread port (from `task_threads()`) to resume
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [thread_resume documentation](https://developer.apple.com/documentation/kernel/1418926-thread_resume/)
    ///
    /// ## Architecture Notes
    ///
    /// - **ARM64**: `thread_resume()` is the preferred method for per-thread control
    /// - **Intel**: You may need to use `thread_set_state()` with `X86_THREAD_STATE64` flavor
    ///   to coordinate per-thread operations (not yet implemented)
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Thread ID is not valid (not in the thread list)
    /// - `ResumeFailed`: `thread_resume()` failed
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ThreadId;
    ///
    /// # let mut debugger = MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let threads = debugger.threads()?;
    /// if let Some(thread) = threads.first() {
    ///     debugger.suspend_thread(*thread)?;
    ///     // ... inspect thread state ...
    ///     debugger.resume_thread(*thread)?;
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    #[allow(unsafe_code)] // Required for thread_resume() call
    pub fn resume_thread(&mut self, thread_id: ThreadId) -> Result<()>
    {
        self.ensure_attached()?;
        threads::ThreadManager::resume_thread(self, thread_id)
    }

    /// Read a 64-bit value from memory at the given address.
    ///
    /// This is a convenience method that reads 8 bytes and interprets them as a little-endian u64.
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Invalid memory address or unable to read 8 bytes
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::Address;
    ///
    /// # let mut debugger = MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let value = debugger.read_memory_u64(Address::from(0x1000))?;
    /// println!("Value at 0x1000: 0x{:016x}", value);
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn read_memory_u64(&self, addr: Address) -> Result<u64>
    {
        self.ensure_attached()?;
        self.memory_cache.read_u64(self.task, addr)
    }

    /// Read a 32-bit value from memory at the given address.
    ///
    /// This is a convenience method that reads 4 bytes and interprets them as a little-endian u32.
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Invalid memory address or unable to read 4 bytes
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::Address;
    ///
    /// # let mut debugger = MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let value = debugger.read_memory_u32(Address::from(0x1000))?;
    /// println!("Value at 0x1000: 0x{:08x}", value);
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn read_memory_u32(&self, addr: Address) -> Result<u32>
    {
        self.ensure_attached()?;
        let bytes = self.memory_cache.read(self.task, addr, 4)?;
        if bytes.len() < 4 {
            return Err(DebuggerError::InvalidArgument(format!(
                "Unable to read 4 bytes from address 0x{:016x}",
                addr.value()
            )));
        }
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Find symbol information for a given address.
    ///
    /// This method symbolicates an address, returning function names and source locations
    /// if available. It automatically loads binary images from memory regions if needed.
    ///
    /// ## Symbolication
    ///
    /// Symbolication maps an address to:
    /// - Function name (demangled if available)
    /// - Source file and line number (if DWARF debug info is present)
    /// - Multiple frames for inlined functions
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Failed to load binary images
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::Address;
    ///
    /// # let mut debugger = MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// if let Some(symbolication) = debugger.find_symbol(Address::from(0x100001000))? {
    ///     for frame in symbolication.frames {
    ///         println!("Function: {}", frame.symbol.display_name());
    ///         if let Some(loc) = frame.location {
    ///             if let Some(line) = loc.line {
    ///                 println!("  {}:{}", loc.file, line);
    ///             } else {
    ///                 println!("  {}", loc.file);
    ///             }
    ///         }
    ///     }
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn find_symbol(&mut self, address: Address) -> Result<Option<crate::symbols::Symbolication>>
    {
        self.ensure_attached()?;

        // Ensure images are loaded (same logic as stack_trace)
        let regions = get_memory_regions(self.task)?;
        
        // Load the main executable if we can find it
        let exec_path = Self::get_executable_path(self.pid);
        if let Some(exec_path) = &exec_path {
            // Find a code region to use as load address
            for region in &regions {
                if region.permissions.contains('x') && region.permissions.contains('r') {
                    let desc = crate::symbols::ImageDescriptor {
                        path: exec_path.clone(),
                        load_address: region.start.value(),
                    };
                    if self.symbol_cache.load_image(desc).is_ok() {
                        break;
                    }
                }
            }
        }

        // Load all other images
        for region in regions {
            if let Some(name) = &region.name
                && name.starts_with('/')
                && (name.ends_with(".dylib") || name.ends_with(".so") || !name.contains('['))
            {
                // Skip if this is the main executable
                if let Some(ref exec_path) = exec_path {
                    if name == exec_path.to_str().unwrap_or("") {
                        continue;
                    }
                }
                let desc = crate::symbols::ImageDescriptor {
                    path: std::path::PathBuf::from(name),
                    load_address: region.start.value(),
                };
                let _ = self.symbol_cache.load_image(desc);
            }
        }

        Ok(self.symbol_cache.symbolicate(address))
    }

    /// Get the executable path for a process using libproc
    fn get_executable_path(pid: ProcessId) -> Option<std::path::PathBuf>
    {
        use libproc::libproc::proc_pid::pidpath;
        match pidpath(pid.0 as i32) {
            Ok(path) => Some(std::path::PathBuf::from(path)),
            Err(_) => None,
        }
    }
}
