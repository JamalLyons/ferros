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
use std::sync::{mpsc, Arc, Mutex};
use std::time::SystemTime;
use std::{mem, thread};

use libc::{c_int, mach_msg_type_number_t, mach_port_t, natural_t, thread_act_t, vm_address_t, vm_size_t};
#[cfg(target_os = "macos")]
use mach2::exc::{__Reply__exception_raise_t, __Request__exception_raise_t};
#[cfg(target_os = "macos")]
use mach2::exception_types::{
    exception_behavior_t, exception_mask_t, exception_type_t, EXCEPTION_DEFAULT, EXC_ARITHMETIC, EXC_BAD_ACCESS,
    EXC_BAD_INSTRUCTION, EXC_BREAKPOINT, EXC_MASK_ARITHMETIC, EXC_MASK_BAD_ACCESS, EXC_MASK_BAD_INSTRUCTION,
    EXC_MASK_BREAKPOINT, EXC_MASK_SOFTWARE, EXC_SOFTWARE, MACH_EXCEPTION_CODES,
};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::mach_port::{mach_port_allocate, mach_port_destroy, mach_port_insert_right};
#[cfg(target_os = "macos")]
use mach2::message::{
    mach_msg, mach_msg_header_t, mach_msg_size_t, MACH_MSGH_BITS, MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE,
    MACH_MSG_TYPE_MAKE_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_RCV_LARGE, MACH_RCV_MSG, MACH_SEND_MSG,
};
#[cfg(target_os = "macos")]
use mach2::ndr::NDR_record;
#[cfg(target_os = "macos")]
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE};
#[cfg(target_os = "macos")]
use mach2::task::{task_resume, task_set_exception_ports, task_suspend, task_threads};
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;
use tracing::warn;

use crate::breakpoints::{
    BreakpointEntry, BreakpointId, BreakpointInfo, BreakpointKind, BreakpointPayload, BreakpointRequest, BreakpointState,
    BreakpointStore,
};
use crate::debugger::Debugger;
use crate::error::{DebuggerError, Result};
use crate::events::{self, DebuggerEvent};
use crate::platform::macos::memory::{get_memory_regions, write_memory, MemoryCache};
#[cfg(target_arch = "aarch64")]
use crate::platform::macos::registers::{read_registers_arm64, write_registers_arm64};
#[cfg(target_arch = "x86_64")]
use crate::platform::macos::registers::{read_registers_x86_64, write_registers_x86_64};
use crate::platform::macos::{debug_registers, ffi};
use crate::symbols::{ImageDescriptor, SymbolCache};
use crate::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, StackFrame, StopReason, ThreadId};
use crate::unwind::{MemoryAccess, StackUnwinder};

/// Shared exception state manipulated by the Mach exception loop and debugger methods.
#[derive(Debug)]
struct ExceptionSharedState
{
    stopped: bool,
    stop_reason: StopReason,
    pending_thread: Option<thread_act_t>,
}

impl ExceptionSharedState
{
    fn new() -> Self
    {
        Self {
            stopped: false,
            stop_reason: StopReason::Running,
            pending_thread: None,
        }
    }
}

#[derive(Debug)]
enum ExceptionLoopCommand
{
    Continue,
    Shutdown,
}

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
    exception_resume_tx: Option<mpsc::Sender<ExceptionLoopCommand>>,
    /// Shared exception state observed by both the handler loop and debugger.
    exception_state: Arc<Mutex<ExceptionSharedState>>,
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
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ProcessId;
    /// use ferros_core::Debugger;
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
            exception_state: Arc::new(Mutex::new(ExceptionSharedState::new())),
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

    fn create_pipe_pair(label: &str) -> Result<(RawFd, RawFd)>
    {
        let mut fds = [0; 2];
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                let err = std::io::Error::last_os_error();
                return Err(DebuggerError::AttachFailed(format!("Failed to create {label} pipe: {err}")));
            }

            for fd in &fds {
                if libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                    let err = std::io::Error::last_os_error();
                    // Best effort cleanup
                    let _ = libc::close(fds[0]);
                    let _ = libc::close(fds[1]);
                    return Err(DebuggerError::AttachFailed(format!(
                        "Failed to configure {label} pipe: {err}"
                    )));
                }
            }
        }

        Ok((fds[0], fds[1]))
    }

    fn close_pipe_pair(pipe: &mut Option<(RawFd, RawFd)>)
    {
        if let Some((read_fd, write_fd)) = pipe.take() {
            unsafe {
                let _ = libc::close(read_fd);
                let _ = libc::close(write_fd);
            }
        }
    }

    fn ensure_file_action_success(
        desc: &str,
        result: c_int,
        attr: &mut libc::posix_spawnattr_t,
        file_actions: &mut libc::posix_spawn_file_actions_t,
        stdout_pipe_fds: &mut Option<(RawFd, RawFd)>,
        stderr_pipe_fds: &mut Option<(RawFd, RawFd)>,
    ) -> Result<()>
    {
        if result == 0 {
            return Ok(());
        }

        let err = std::io::Error::from_raw_os_error(result);
        unsafe {
            let _ = libc::posix_spawn_file_actions_destroy(file_actions);
        }
        unsafe {
            let _ = ffi::posix_spawnattr_destroy(attr);
        }
        Self::close_pipe_pair(stdout_pipe_fds);
        Self::close_pipe_pair(stderr_pipe_fds);
        Err(DebuggerError::AttachFailed(format!("Failed to {desc}: {err}")))
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
                Self::deallocate_threads_array(threads, thread_count);
                return Err(DebuggerError::AttachFailed(format!("Failed to get threads: {}", result)));
            }

            let slice = std::slice::from_raw_parts(threads, thread_count as usize);
            self.task = task;
            self.pid = pid;
            self.threads = slice.to_vec();
            Self::deallocate_threads_array(threads, thread_count);
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
        self.current_thread
            .ok_or_else(|| DebuggerError::InvalidArgument("No active thread selected".to_string()))
    }

    fn thread_port_for_id(&self, thread: ThreadId) -> Result<thread_act_t>
    {
        self.ensure_attached()?;
        let port = thread.raw() as thread_act_t;
        if self.threads.contains(&port) {
            Ok(port)
        } else {
            Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of process {}. Call refresh_threads() to update the thread list.",
                thread.raw(),
                self.pid.0
            )))
        }
    }

    fn software_trap_bytes(&self) -> Result<Vec<u8>>
    {
        match self.architecture {
            Architecture::Arm64 => Ok(vec![0x00, 0x00, 0x20, 0xD4]), // BRK #0
            Architecture::X86_64 => Ok(vec![0xCC]),                  // INT3
            Architecture::Unknown(label) => Err(DebuggerError::InvalidArgument(format!(
                "Software breakpoints unsupported for architecture: {label}"
            ))),
        }
    }

    fn install_software_breakpoint(&mut self, address: Address) -> Result<BreakpointId>
    {
        self.ensure_attached()?;
        let trap = self.software_trap_bytes()?;

        {
            let store = self.breakpoints.lock().unwrap();
            if store.id_for_kind(address, BreakpointKind::Software).is_some() {
                return Err(DebuggerError::InvalidArgument(format!(
                    "Breakpoint already exists at 0x{:016x}",
                    address.value()
                )));
            }
        }

        let original = self.read_memory(address, trap.len())?;
        if original.len() != trap.len() {
            return Err(DebuggerError::InvalidArgument(format!(
                "Unable to read {} bytes at 0x{:016x} to install breakpoint",
                trap.len(),
                address.value()
            )));
        }

        let written = self.write_memory(address, &trap)?;
        if written != trap.len() {
            return Err(DebuggerError::InvalidArgument(format!(
                "Failed to write breakpoint trap at 0x{:016x}",
                address.value()
            )));
        }

        let mut info = BreakpointInfo::new(BreakpointId::from_raw(0), address, BreakpointKind::Software);
        info.state = BreakpointState::Resolved;
        info.enabled = true;
        info.resolved_at = Some(SystemTime::now());

        let entry = BreakpointEntry {
            info,
            payload: BreakpointPayload::Software {
                original_bytes: original,
            },
        };

        let mut store = self.breakpoints.lock().unwrap();
        Ok(store.insert(entry))
    }

    fn install_hardware_breakpoint(&mut self, address: Address) -> Result<BreakpointId>
    {
        self.ensure_attached()?;

        {
            let store = self.breakpoints.lock().unwrap();
            if store.id_for_kind(address, BreakpointKind::Hardware).is_some() {
                return Err(DebuggerError::InvalidArgument(format!(
                    "Hardware breakpoint already exists at 0x{:016x}",
                    address.value()
                )));
            }
        }

        // Install on all threads
        let mut used_slot = None;

        for &thread in &self.threads {
            let slot = debug_registers::set_hardware_breakpoint(thread, address)?;
            if let Some(s) = used_slot {
                if s != slot {
                    tracing::warn!("Hardware breakpoint slots inconsistent across threads: {} vs {}", s, slot);
                }
            } else {
                used_slot = Some(slot);
            }
        }

        let slot = used_slot
            .ok_or_else(|| DebuggerError::AttachFailed("No threads available to set hardware breakpoint".into()))?;

        let mut info = BreakpointInfo::new(BreakpointId::from_raw(0), address, BreakpointKind::Hardware);
        info.state = BreakpointState::Resolved;
        info.enabled = true;
        info.resolved_at = Some(SystemTime::now());

        let entry = BreakpointEntry {
            info,
            payload: BreakpointPayload::Hardware { address, slot },
        };

        let mut store = self.breakpoints.lock().unwrap();
        Ok(store.insert(entry))
    }

    fn restore_software_breakpoint(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        if let BreakpointPayload::Software { original_bytes } = &entry.payload {
            let written = self.write_memory(entry.info.address, original_bytes)?;
            if written != original_bytes.len() {
                return Err(DebuggerError::InvalidArgument(format!(
                    "Failed to restore original instruction at 0x{:016x}",
                    entry.info.address.value()
                )));
            }
        }
        Ok(())
    }

    fn remove_hardware_breakpoint(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        if let BreakpointPayload::Hardware { slot, .. } = &entry.payload {
            for &thread in &self.threads {
                // Best effort removal
                if let Err(e) = debug_registers::clear_hardware_breakpoint(thread, *slot) {
                    tracing::warn!("Failed to clear hardware breakpoint on thread {}: {}", thread, e);
                }
            }
        }
        Ok(())
    }

    fn restore_all_breakpoints(&mut self)
    {
        let entries = {
            let mut store = self.breakpoints.lock().unwrap();
            store.drain()
        };

        for entry in entries {
            if let BreakpointPayload::Software { .. } = entry.payload {
                if let Err(err) = self.restore_software_breakpoint(&entry) {
                    warn!("Failed to restore breakpoint 0x{:016x}: {err}", entry.info.address.value());
                }
            } else if let BreakpointPayload::Hardware { .. } = entry.payload {
                if let Err(err) = self.remove_hardware_breakpoint(&entry) {
                    warn!(
                        "Failed to remove hardware breakpoint 0x{:016x}: {err}",
                        entry.info.address.value()
                    );
                }
            }
        }
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
        if self.threads.contains(&port) {
            self.current_thread = Some(port);
            Ok(())
        } else {
            Err(DebuggerError::InvalidArgument(format!(
                "Thread {port} is not part of process {}",
                self.pid.0
            )))
        }
    }

    /// Deallocate memory allocated by `task_threads()`
    ///
    /// This is an internal helper method that frees the memory allocated by the
    /// Mach API `task_threads()`. The Mach API allocates memory for the thread
    /// array that must be freed using `vm_deallocate()`.
    ///
    /// ## Parameters
    ///
    /// - `threads`: Pointer to the thread array allocated by `task_threads()`
    /// - `count`: Number of threads in the array
    ///
    /// ## Safety
    ///
    /// This function is safe to call with any pointer and count. It checks for null
    /// pointers and zero counts before attempting to deallocate.
    ///
    /// ## Mach API
    ///
    /// Uses `vm_deallocate()` to free the memory:
    /// - See: [vm_deallocate documentation](https://developer.apple.com/documentation/kernel/1585284-vm_deallocate/)
    fn deallocate_threads_array(threads: *mut thread_act_t, count: mach_msg_type_number_t)
    {
        if threads.is_null() || count == 0 {
            return;
        }

        let size = (count as usize).saturating_mul(mem::size_of::<thread_act_t>()) as vm_size_t;
        unsafe {
            let _ = ffi::vm_deallocate(mach_task_self(), threads as vm_address_t, size);
        }
    }

    /// Refresh the thread list from the operating system
    ///
    /// This is an internal helper method that updates the cached thread list by
    /// calling `task_threads()` to get the current set of threads in the target
    /// process. It also updates the active thread if the current one no longer exists.
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
    /// **Returns**: Array of thread ports. The memory must be freed using `vm_deallocate()`.
    ///
    /// See: [task_threads documentation](https://developer.apple.com/documentation/kernel/1537751-task_threads/)
    ///
    /// ## Implementation Notes
    ///
    /// - Deallocates old thread ports before getting new ones to prevent port leaks
    /// - Updates the active thread to the first thread if the current one no longer exists
    ///
    /// ## Errors
    ///
    /// Returns `NotAttached` if not attached to a process.
    /// Returns `AttachFailed` if `task_threads()` fails or no threads found.
    ///
    /// ## See Also
    ///
    /// - `refresh_threads()`: Public API that calls this method
    fn refresh_thread_list(&mut self) -> Result<()>
    {
        self.ensure_attached()?;

        unsafe {
            // Deallocate old thread ports before getting new ones to prevent port leaks
            for thread in &self.threads {
                let _ = ffi::mach_port_deallocate(mach_task_self(), *thread);
            }

            let mut threads: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;
            let result = task_threads(self.task, &mut threads, &mut thread_count);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to enumerate threads: {}",
                    result
                )));
            }

            let slice = std::slice::from_raw_parts(threads, thread_count as usize);
            self.threads = slice.to_vec();
            Self::deallocate_threads_array(threads, thread_count);

            // Update active thread - use first thread if current one no longer exists
            if let Some(current) = self.current_thread {
                if !self.threads.contains(&current) {
                    self.current_thread = self.threads.first().copied();
                }
            } else {
                self.current_thread = self.threads.first().copied();
            }
        }

        Ok(())
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
            .send(ExceptionLoopCommand::Continue)
            .map_err(|_| DebuggerError::ResumeFailed("failed to signal exception handler".to_string()))?;

        Ok(true)
    }

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
                let flavor = thread_state_flavor_for_arch(self.architecture);
                kr = task_set_exception_ports(self.task, mask, port, behavior, flavor);
                if kr != KERN_SUCCESS {
                    let _ = mach_port_destroy(self_task, port);
                    return Err(DebuggerError::MachError(kr.into()));
                }

                let (tx, rx) = mpsc::channel();
                let shared_state = Arc::clone(&self.exception_state);
                let breakpoints = Arc::clone(&self.breakpoints);
                let architecture = self.architecture;
                let event_tx = self.event_tx.clone();
                info!("Spawning Mach exception handler thread");
                let handle = thread::Builder::new()
                    .name("ferros-mac-exc".to_string())
                    .spawn(move || run_exception_loop(port, rx, shared_state, architecture, event_tx, breakpoints))
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
        match request {
            BreakpointRequest::Software { address } => self.install_software_breakpoint(address),
            BreakpointRequest::Hardware { address } => self.install_hardware_breakpoint(address),
            BreakpointRequest::Watchpoint { .. } => Err(DebuggerError::InvalidArgument(
                "Watchpoints are not yet supported on macOS".to_string(),
            )),
        }
    }

    fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        let entry = {
            let mut store = self.breakpoints.lock().unwrap();
            store.remove(id)
        }
        .ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?;

        if entry.info.enabled {
            match entry.info.kind {
                BreakpointKind::Software => self.restore_software_breakpoint(&entry)?,
                BreakpointKind::Hardware => self.remove_hardware_breakpoint(&entry)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn enable_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        let (kind, address) = {
            let store = self.breakpoints.lock().unwrap();
            let entry = store.get(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?;
            if entry.info.enabled {
                return Ok(());
            }
            (entry.info.kind, entry.info.address)
        };

        match kind {
            BreakpointKind::Software => {
                let trap = self.software_trap_bytes()?;
                let written = self.write_memory(address, &trap)?;
                if written != trap.len() {
                    return Err(DebuggerError::InvalidArgument(format!(
                        "Failed to re-arm breakpoint at 0x{:016x}",
                        address.value()
                    )));
                }
            }
            BreakpointKind::Hardware => {
                // Re-install on all threads
                let mut used_slot = None;
                for &thread in &self.threads {
                    let slot = debug_registers::set_hardware_breakpoint(thread, address)?;
                    if let Some(s) = used_slot {
                        if s != slot {
                            tracing::warn!("Hardware breakpoint slots inconsistent: {} vs {}", s, slot);
                        }
                    } else {
                        used_slot = Some(slot);
                    }
                }

                if let Some(new_slot) = used_slot {
                    let mut store = self.breakpoints.lock().unwrap();
                    if let Some(entry) = store.get_mut(id) {
                        entry.payload = BreakpointPayload::Hardware { address, slot: new_slot };
                    }
                }
            }
            _ => return Err(DebuggerError::InvalidArgument("Watchpoints not supported".into())),
        }

        let mut store = self.breakpoints.lock().unwrap();
        if let Some(entry) = store.get_mut(id) {
            entry.info.state = BreakpointState::Resolved;
            entry.info.enabled = true;
            entry.info.resolved_at = Some(SystemTime::now());
        }
        Ok(())
    }

    fn disable_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        let (kind, address, payload) = {
            let store = self.breakpoints.lock().unwrap();
            let entry = store.get(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?;
            if !entry.info.enabled {
                return Ok(());
            }
            (entry.info.kind, entry.info.address, entry.payload.clone())
        };

        match kind {
            BreakpointKind::Software => {
                if let BreakpointPayload::Software { original_bytes } = payload {
                    let written = self.write_memory(address, &original_bytes)?;
                    if written != original_bytes.len() {
                        return Err(DebuggerError::InvalidArgument(format!(
                            "Failed to disable breakpoint at 0x{:016x}",
                            address.value()
                        )));
                    }
                }
            }
            BreakpointKind::Hardware => {
                if let BreakpointPayload::Hardware { slot, .. } = payload {
                    for &thread in &self.threads {
                        if let Err(e) = debug_registers::clear_hardware_breakpoint(thread, slot) {
                            tracing::warn!("Failed to clear hardware breakpoint: {}", e);
                        }
                    }
                }
            }
            _ => {}
        }

        let mut store = self.breakpoints.lock().unwrap();
        if let Some(entry) = store.get_mut(id) {
            entry.info.state = BreakpointState::Disabled;
            entry.info.enabled = false;
        }
        Ok(())
    }

    fn toggle_breakpoint(&mut self, id: BreakpointId) -> Result<bool>
    {
        let enabled = {
            let store = self.breakpoints.lock().unwrap();
            store
                .get(id)
                .ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?
                .info
                .enabled
        };
        if enabled {
            self.disable_breakpoint(id)?;
            Ok(false)
        } else {
            self.enable_breakpoint(id)?;
            Ok(true)
        }
    }

    fn breakpoint_info(&self, id: BreakpointId) -> Result<BreakpointInfo>
    {
        let store = self.breakpoints.lock().unwrap();
        store.info(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))
    }

    fn breakpoints(&self) -> Vec<BreakpointInfo>
    {
        self.breakpoints.lock().unwrap().list()
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
        for region in regions {
            if let Some(name) = &region.name {
                if name.starts_with('/') && (name.ends_with(".dylib") || name.ends_with(".so") || !name.contains('[')) {
                    let desc = ImageDescriptor {
                        path: std::path::PathBuf::from(name),
                        load_address: region.start.value(),
                    };
                    let _ = self.symbol_cache.load_image(desc);
                }
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
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::Debugger;
    ///
    /// let mut debugger = MacOSDebugger::new()?;
    /// debugger.launch("/usr/bin/echo", &["echo", "Hello, world!"])?;
    /// // Process is now suspended and ready for debugging
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn launch(&mut self, program: &str, args: &[&str]) -> Result<ProcessId>
    {
        use std::ffi::CString;
        use std::ptr;

        use tracing::{debug, info, trace};

        info!("Launching process: {} with args: {:?}", program, args);
        debug!("Validating launch parameters");

        // Validate inputs
        if program.is_empty() {
            return Err(DebuggerError::InvalidArgument("Program path cannot be empty".to_string()));
        }
        if args.is_empty() {
            return Err(DebuggerError::InvalidArgument("Arguments cannot be empty".to_string()));
        }

        // Convert program path to CString
        let program_cstr =
            CString::new(program).map_err(|e| DebuggerError::InvalidArgument(format!("Invalid program path: {}", e)))?;

        // Convert arguments to CStrings
        let mut arg_cstrs = Vec::new();
        for arg in args {
            arg_cstrs
                .push(CString::new(*arg).map_err(|e| DebuggerError::InvalidArgument(format!("Invalid argument: {}", e)))?);
        }

        // Create argv array (null-terminated)
        let mut argv: Vec<*const libc::c_char> = arg_cstrs.iter().map(|s| s.as_ptr()).collect();
        argv.push(ptr::null());

        let mut stdout_pipe_fds: Option<(RawFd, RawFd)> = None;
        let mut stderr_pipe_fds: Option<(RawFd, RawFd)> = None;

        if self.capture_output {
            stdout_pipe_fds = Some(Self::create_pipe_pair("stdout")?);
            stderr_pipe_fds = Some(Self::create_pipe_pair("stderr")?);
        }

        unsafe {
            debug!("Initializing posix_spawn attributes");
            // Initialize spawn attributes
            let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
            let result = ffi::posix_spawnattr_init(&mut attr);
            if result != 0 {
                Self::close_pipe_pair(&mut stdout_pipe_fds);
                Self::close_pipe_pair(&mut stderr_pipe_fds);
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to initialize spawn attributes: {}",
                    std::io::Error::from_raw_os_error(result)
                )));
            }

            debug!("Setting POSIX_SPAWN_START_SUSPENDED flag");
            // Set POSIX_SPAWN_START_SUSPENDED flag
            let flags_result = ffi::posix_spawnattr_setflags(&mut attr, ffi::spawn_flags::POSIX_SPAWN_START_SUSPENDED);
            if flags_result != 0 {
                let _ = ffi::posix_spawnattr_destroy(&mut attr);
                Self::close_pipe_pair(&mut stdout_pipe_fds);
                Self::close_pipe_pair(&mut stderr_pipe_fds);
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to set spawn flags: {}",
                    std::io::Error::from_raw_os_error(flags_result)
                )));
            }

            let mut file_actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
            let mut file_actions_initialized = false;

            if self.capture_output {
                file_actions_initialized = true;
                let init_result = libc::posix_spawn_file_actions_init(&mut file_actions);
                if init_result != 0 {
                    let _ = ffi::posix_spawnattr_destroy(&mut attr);
                    Self::close_pipe_pair(&mut stdout_pipe_fds);
                    Self::close_pipe_pair(&mut stderr_pipe_fds);
                    return Err(DebuggerError::AttachFailed(format!(
                        "Failed to initialize file actions: {}",
                        std::io::Error::from_raw_os_error(init_result)
                    )));
                }

                if let Some((read_fd, write_fd)) = stdout_pipe_fds {
                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, read_fd);
                    Self::ensure_file_action_success(
                        "close stdout read end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_adddup2(&mut file_actions, write_fd, libc::STDOUT_FILENO);
                    Self::ensure_file_action_success(
                        "redirect stdout",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, write_fd);
                    Self::ensure_file_action_success(
                        "close stdout write end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;
                }

                if let Some((read_fd, write_fd)) = stderr_pipe_fds {
                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, read_fd);
                    Self::ensure_file_action_success(
                        "close stderr read end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_adddup2(&mut file_actions, write_fd, libc::STDERR_FILENO);
                    Self::ensure_file_action_success(
                        "redirect stderr",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, write_fd);
                    Self::ensure_file_action_success(
                        "close stderr write end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;
                }
            }

            trace!("Calling posix_spawn");
            // Spawn the process
            let mut pid: libc::pid_t = 0;
            let spawn_result = ffi::posix_spawn(
                &mut pid,
                program_cstr.as_ptr(),
                if file_actions_initialized {
                    &file_actions
                } else {
                    ptr::null()
                },
                &attr, // Use attributes with START_SUSPENDED flag
                argv.as_ptr(),
                ptr::null(), // Use current environment
            );

            if file_actions_initialized {
                let _ = libc::posix_spawn_file_actions_destroy(&mut file_actions);
            }

            // Clean up attributes
            let _ = ffi::posix_spawnattr_destroy(&mut attr);

            if spawn_result != 0 {
                Self::close_pipe_pair(&mut stdout_pipe_fds);
                Self::close_pipe_pair(&mut stderr_pipe_fds);
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to spawn process '{}': {}",
                    program,
                    std::io::Error::from_raw_os_error(spawn_result)
                )));
            }

            if let Some((read_fd, write_fd)) = stdout_pipe_fds.take() {
                let _ = libc::close(write_fd);
                self.stdout_pipe = Some(File::from_raw_fd(read_fd));
            }

            if let Some((read_fd, write_fd)) = stderr_pipe_fds.take() {
                let _ = libc::close(write_fd);
                self.stderr_pipe = Some(File::from_raw_fd(read_fd));
            }

            info!("Successfully spawned process with PID: {}", pid);
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

#[cfg(target_os = "macos")]
fn run_exception_loop(
    exception_port: mach_port_t,
    resume_rx: mpsc::Receiver<ExceptionLoopCommand>,
    shared_state: Arc<Mutex<ExceptionSharedState>>,
    architecture: Architecture,
    event_tx: events::DebuggerEventSender,
    breakpoints: Arc<Mutex<BreakpointStore>>,
)
{
    use std::mem::MaybeUninit;

    use tracing::{debug, error};

    loop {
        let mut request = MaybeUninit::<__Request__exception_raise_t>::uninit();
        let recv_size = std::mem::size_of::<__Request__exception_raise_t>() as mach_msg_size_t;

        let kr = unsafe {
            mach_msg(
                request.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_RCV_MSG | MACH_RCV_LARGE,
                0,
                recv_size,
                exception_port,
                MACH_MSG_TIMEOUT_NONE,
                MACH_PORT_NULL,
            )
        };

        if kr != MACH_MSG_SUCCESS {
            if kr == mach2::message::MACH_RCV_PORT_DIED || kr == mach2::message::MACH_RCV_INVALID_NAME {
                debug!("Mach exception port closed, exiting handler loop");
                break;
            }
            continue;
        }

        let message = unsafe { request.assume_init() };
        let thread_port = message.thread.name as thread_act_t;
        let codes = [message.code[0] as i64, message.code[1] as i64];

        let rewound_pc = if message.exception == EXC_BREAKPOINT as exception_type_t {
            match rewind_breakpoint_pc(thread_port, architecture) {
                Ok(value) => value,
                Err(err) => {
                    error!("Failed to rewind breakpoint PC: {err}");
                    None
                }
            }
        } else {
            None
        };

        let stop_reason = stop_reason_from_exception(message.exception, rewound_pc, codes);
        {
            let mut shared = shared_state.lock().unwrap();
            shared.stopped = true;
            shared.stop_reason = stop_reason;
            shared.pending_thread = Some(thread_port);
        }

        if let StopReason::Breakpoint(addr) = stop_reason {
            let mut store = breakpoints.lock().unwrap();
            store.record_hit(Address::from(addr));
        }

        if let Err(err) = event_tx.send(DebuggerEvent::TargetStopped {
            reason: stop_reason,
            thread: Some(ThreadId::from(thread_port as u64)),
        }) {
            tracing::warn!("Failed to send stop event from Mach loop: {err}");
        }

        match resume_rx.recv() {
            Ok(ExceptionLoopCommand::Continue) => {
                if let Err(err) = send_exception_reply(&message) {
                    error!("Failed to send Mach exception reply: {err}");
                    break;
                }

                let mut shared = shared_state.lock().unwrap();
                shared.stopped = false;
                shared.stop_reason = StopReason::Running;
                shared.pending_thread = None;

                if let Err(err) = event_tx.send(DebuggerEvent::TargetResumed) {
                    tracing::warn!("Failed to send resume event from Mach loop: {err}");
                }
            }
            Ok(ExceptionLoopCommand::Shutdown) | Err(_) => {
                let mut shared = shared_state.lock().unwrap();
                shared.stopped = false;
                shared.stop_reason = StopReason::Running;
                shared.pending_thread = None;
                break;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn send_exception_reply(request: &__Request__exception_raise_t) -> Result<()>
{
    let mut reply = __Reply__exception_raise_t {
        Head: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
            msgh_size: std::mem::size_of::<__Reply__exception_raise_t>() as mach_msg_size_t,
            msgh_remote_port: request.Head.msgh_local_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: request.Head.msgh_id + 100,
        },
        NDR: unsafe { NDR_record },
        RetCode: KERN_SUCCESS,
    };

    let kr = unsafe {
        mach_msg(
            &mut reply.Head,
            MACH_SEND_MSG,
            reply.Head.msgh_size,
            0,
            MACH_PORT_NULL,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };

    if kr != MACH_MSG_SUCCESS {
        return Err(DebuggerError::ResumeFailed(format!("mach_msg reply failed: {}", kr)));
    }

    Ok(())
}

fn thread_state_flavor_for_arch(architecture: Architecture) -> c_int
{
    match architecture {
        Architecture::Arm64 => 6,
        Architecture::X86_64 => 4,
        Architecture::Unknown(_) => 6,
    }
}

fn rewind_breakpoint_pc(thread: thread_act_t, architecture: Architecture) -> Result<Option<u64>>
{
    match architecture {
        Architecture::Arm64 => rewind_breakpoint_pc_arm64(thread),
        Architecture::X86_64 => rewind_breakpoint_pc_x86(thread),
        Architecture::Unknown(_) => Ok(None),
    }
}

#[cfg(target_arch = "aarch64")]
fn rewind_breakpoint_pc_arm64(thread: thread_act_t) -> Result<Option<u64>>
{
    const ARM_THREAD_STATE64: c_int = 6;
    const ARM_THREAD_STATE64_COUNT: mach_msg_type_number_t = 68;
    const INSTRUCTION_SIZE: u64 = 4;

    unsafe {
        let mut state: [natural_t; ARM_THREAD_STATE64_COUNT as usize] = [0; ARM_THREAD_STATE64_COUNT as usize];
        let mut count = ARM_THREAD_STATE64_COUNT;
        let mut kr = ffi::thread_get_state(thread, ARM_THREAD_STATE64, state.as_mut_ptr(), &mut count);
        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        let read_u64 = |idx: usize, buf: &[natural_t]| -> u64 {
            let lo = buf[idx * 2] as u64;
            let hi = buf[idx * 2 + 1] as u64;
            lo | (hi << 32)
        };

        let pc = read_u64(32, &state);
        let new_pc = pc.saturating_sub(INSTRUCTION_SIZE);
        state[64] = (new_pc & 0xFFFF_FFFF) as natural_t;
        state[65] = (new_pc >> 32) as natural_t;

        kr = ffi::thread_set_state(thread, ARM_THREAD_STATE64, state.as_ptr(), ARM_THREAD_STATE64_COUNT);
        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(Some(new_pc))
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn rewind_breakpoint_pc_arm64(_thread: thread_act_t) -> Result<Option<u64>>
{
    Ok(None)
}

#[cfg(target_arch = "x86_64")]
fn rewind_breakpoint_pc_x86(thread: thread_act_t) -> Result<Option<u64>>
{
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct X86ThreadState64
    {
        rax: u64,
        rbx: u64,
        rcx: u64,
        rdx: u64,
        rdi: u64,
        rsi: u64,
        rbp: u64,
        rsp: u64,
        r8: u64,
        r9: u64,
        r10: u64,
        r11: u64,
        r12: u64,
        r13: u64,
        r14: u64,
        r15: u64,
        rip: u64,
        rflags: u64,
        cs: u64,
        fs: u64,
        gs: u64,
    }

    const X86_THREAD_STATE64: c_int = 4;
    const X86_THREAD_STATE64_COUNT: mach_msg_type_number_t = 42;
    const INSTRUCTION_SIZE: u64 = 1;

    unsafe {
        let mut state = X86ThreadState64::default();
        let mut count = X86_THREAD_STATE64_COUNT;
        let mut kr = ffi::thread_get_state(thread, X86_THREAD_STATE64, &mut state as *mut _ as *mut natural_t, &mut count);

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        let new_pc = state.rip.saturating_sub(INSTRUCTION_SIZE);
        state.rip = new_pc;

        kr = ffi::thread_set_state(
            thread,
            X86_THREAD_STATE64,
            &state as *const _ as *const natural_t,
            X86_THREAD_STATE64_COUNT,
        );
        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(Some(new_pc))
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn rewind_breakpoint_pc_x86(_thread: thread_act_t) -> Result<Option<u64>>
{
    Ok(None)
}

fn stop_reason_from_exception(exception: exception_type_t, pc: Option<u64>, _codes: [i64; 2]) -> StopReason
{
    match exception as u32 {
        EXC_BREAKPOINT => StopReason::Breakpoint(pc.unwrap_or(0)),
        EXC_BAD_ACCESS => StopReason::Signal(libc::SIGSEGV),
        EXC_BAD_INSTRUCTION => StopReason::Signal(libc::SIGILL),
        EXC_ARITHMETIC => StopReason::Signal(libc::SIGFPE),
        EXC_SOFTWARE => StopReason::Signal(libc::SIGTRAP),
        _ => StopReason::Unknown,
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
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ThreadId;
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let threads = debugger.threads()?;
    /// if let Some(thread) = threads.first() {
    ///     debugger.suspend_thread(*thread)?;
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    #[allow(unsafe_code)] // Required for thread_suspend() call
    pub fn suspend_thread(&mut self, thread_id: ThreadId) -> Result<()>
    {
        self.ensure_attached()?;

        let thread_port = thread_id.raw() as thread_act_t;
        if !self.threads.contains(&thread_port) {
            return Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of process {}",
                thread_id.raw(),
                self.pid.0
            )));
        }

        unsafe {
            let result = ffi::thread_suspend(thread_port);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::SuspendFailed(format!(
                    "thread_suspend failed for thread {}: {}",
                    thread_id.raw(),
                    result
                )));
            }
        }

        Ok(())
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
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ThreadId;
    /// use ferros_core::Debugger;
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

        let thread_port = thread_id.raw() as thread_act_t;
        if !self.threads.contains(&thread_port) {
            return Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of process {}",
                thread_id.raw(),
                self.pid.0
            )));
        }

        unsafe {
            let result = ffi::thread_resume(thread_port);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::ResumeFailed(format!(
                    "thread_resume failed for thread {}: {}",
                    thread_id.raw(),
                    result
                )));
            }
        }

        Ok(())
    }
}
