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

use std::mem;

use libc::{c_int, mach_msg_type_number_t, mach_port_t, thread_act_t, vm_address_t, vm_size_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::task::{task_resume, task_suspend, task_threads};
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;

use crate::debugger::Debugger;
use crate::error::{DebuggerError, Result};
use crate::platform::macos::ffi;
use crate::platform::macos::memory::{get_memory_regions, read_memory, write_memory};
#[cfg(target_arch = "aarch64")]
use crate::platform::macos::registers::read_registers_arm64;
#[cfg(target_arch = "x86_64")]
use crate::platform::macos::registers::read_registers_x86_64;
use crate::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, StopReason, ThreadId};

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
    /// Whether the process is currently suspended.
    stopped: bool,
    /// Last known stop reason.
    stop_reason: StopReason,
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
        Ok(Self {
            task: 0,
            threads: Vec::new(),
            current_thread: None,
            pid: ProcessId(0),
            architecture: Architecture::current(),
            attached: false,
            stopped: false,
            stop_reason: StopReason::Running,
        })
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
    /// ```rust,no_run
    /// # use ferros_core::platform::macos::MacOSDebugger;
    /// # let debugger = MacOSDebugger::new()?;
    /// // This would fail if not attached
    /// debugger.ensure_attached()?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
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
    /// See: [task_threads documentation](https://developer.apple.com/documentation/kernel/1402802-task_threads)
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
            self.current_thread = self.threads.first().copied();
        }

        Ok(())
    }
}

impl Debugger for MacOSDebugger
{
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
    #[allow(unsafe_code)] // Required for Mach API calls (task_for_pid, task_threads)
    fn attach(&mut self, pid: ProcessId) -> Result<()>
    {
        // Use mach2 crate for Mach APIs where available - it's better maintained than libc
        // mach2 provides:
        // - mach_task_self(): Get our own task port (not deprecated like libc's version)
        // - task_threads(): Enumerate threads in a task
        //
        unsafe {
            // Step 1: Get a Mach port to the target process
            // mach_task_self() returns our own task port (from mach2, not deprecated)
            // task_for_pid() requires special permissions (sudo or debugging entitlements)
            //
            // See: XNU kernel source for task_for_pid implementation
            let mut task: mach_port_t = 0;
            let result = ffi::task_for_pid(mach_task_self(), pid.0 as c_int, &mut task);

            // Check if task_for_pid succeeded
            // KERN_SUCCESS = 0, anything else is an error
            if result != KERN_SUCCESS {
                // macOS quirk: task_for_pid() sometimes returns KERN_FAILURE instead of
                // KERN_PROTECTION_FAILURE when permissions are denied. If the process exists
                // but we got KERN_FAILURE, it's likely a permission issue.
                if result == libc::KERN_FAILURE {
                    // Check if process exists using kill(pid, 0)
                    // Signal 0 doesn't send a signal, it just checks if process exists
                    let process_exists = libc::kill(pid.0 as libc::pid_t, 0) == 0;

                    if process_exists {
                        // Process exists but we got KERN_FAILURE -> permission denied
                        return Err(DebuggerError::PermissionDenied(format!(
                            "task_for_pid() failed with KERN_FAILURE, but process {} exists. This usually means \
                             insufficient permissions. Try running with sudo.",
                            pid.0
                        )));
                    }
                    // Process doesn't exist -> genuine ProcessNotFound
                }

                return Err(DebuggerError::MachError(result.into()));
            }

            // Step 2: Get all threads in the task
            // We need a thread port to read/write registers
            // task_threads() returns an array of thread ports
            //
            // See: XNU kernel source for task_threads implementation
            let mut threads: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;

            let result = task_threads(task, &mut threads, &mut thread_count);
            if result != KERN_SUCCESS || thread_count == 0 {
                Self::deallocate_threads_array(threads, thread_count);
                return Err(DebuggerError::AttachFailed(format!("Failed to get threads: {}", result)));
            }

            // Step 3: Store the task port, PID, and thread list
            let slice = std::slice::from_raw_parts(threads, thread_count as usize);
            self.task = task;
            self.pid = pid;
            self.threads = slice.to_vec();
            Self::deallocate_threads_array(threads, thread_count);
            self.current_thread = self.threads.first().copied();
            self.attached = true;
            self.stopped = false;
            self.stop_reason = StopReason::Running;

            // Suspend immediately so the debugger has control.
            self.suspend()?;

            Ok(())
        }
    }

    /// Detach from the process
    ///
    /// On macOS, we don't actually need to do anything - the Mach ports
    /// are automatically released when the struct is dropped. But we provide
    /// this method for consistency with other platforms and to allow explicit
    /// cleanup.
    ///
    /// ## Note
    ///
    /// Unlike Linux's `ptrace`, macOS doesn't have an explicit "detach" operation.
    /// The task port is just a reference - when we stop using it, the kernel
    /// automatically cleans it up.
    fn detach(&mut self) -> Result<()>
    {
        self.task = 0;
        self.threads.clear();
        self.current_thread = None;
        self.pid = ProcessId(0);
        self.attached = false;
        self.stopped = false;
        self.stop_reason = StopReason::Running;
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

    /// Write registers to the attached process
    ///
    /// **Not yet implemented** - will use `thread_set_state()` when ready.
    ///
    /// ## Future Implementation
    ///
    /// Will call `thread_set_state()` with the new register values.
    /// This requires careful handling of the thread state structure.
    fn write_registers(&mut self, _regs: &Registers) -> Result<()>
    {
        // TODO: Implement register writing using thread_set_state()
        Err(DebuggerError::InvalidArgument(
            "Register writing not yet implemented".to_string(),
        ))
    }

    /// Read memory from the target process
    ///
    /// Uses `vm_read()` to read memory from the Mach task.
    fn read_memory(&self, addr: Address, len: usize) -> Result<Vec<u8>>
    {
        self.ensure_attached()?;
        read_memory(self.task, addr, len)
    }

    /// Write memory to the target process
    ///
    /// Uses `vm_write()` to write memory to the Mach task.
    fn write_memory(&mut self, addr: Address, data: &[u8]) -> Result<usize>
    {
        self.ensure_attached()?;
        write_memory(self.task, addr, data)
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
        self.stopped
    }

    fn stop_reason(&self) -> StopReason
    {
        self.stop_reason
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
        self.ensure_attached()?;
        if self.stopped {
            return Ok(());
        }

        unsafe {
            let result = task_suspend(self.task);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::SuspendFailed(format!("task_suspend failed: {}", result)));
            }
        }

        self.stopped = true;
        self.stop_reason = StopReason::Suspended;
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
        self.ensure_attached()?;
        if !self.stopped {
            return Ok(());
        }

        unsafe {
            let result = task_resume(self.task);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::ResumeFailed(format!("task_resume failed: {}", result)));
            }
        }

        self.stopped = false;
        self.stop_reason = StopReason::Running;
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
