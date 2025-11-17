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

use std::convert::TryInto;
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
use crate::types::{Architecture, MemoryRegion, ProcessId, Registers, StopReason, ThreadId};

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

    fn ensure_attached(&self) -> Result<()>
    {
        if !self.attached || self.task == 0 {
            return Err(DebuggerError::AttachFailed("Not attached to a process".to_string()));
        }
        Ok(())
    }

    fn active_thread_port(&self) -> Result<thread_act_t>
    {
        self.current_thread
            .ok_or_else(|| DebuggerError::InvalidArgument("No active thread selected".to_string()))
    }

    fn set_active_thread_by_port(&mut self, port: thread_act_t) -> Result<()>
    {
        if self.threads.iter().any(|&t| t == port) {
            self.current_thread = Some(port);
            Ok(())
        } else {
            Err(DebuggerError::InvalidArgument(format!(
                "Thread {port} is not part of process {}",
                self.pid.0
            )))
        }
    }

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
    fn read_memory(&self, addr: u64, len: usize) -> Result<Vec<u8>>
    {
        self.ensure_attached()?;
        read_memory(self.task, addr, len)
    }

    /// Write memory to the target process
    ///
    /// Uses `vm_write()` to write memory to the Mach task.
    fn write_memory(&mut self, addr: u64, data: &[u8]) -> Result<usize>
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

    fn suspend(&mut self) -> Result<()>
    {
        self.ensure_attached()?;
        if self.stopped {
            return Ok(());
        }

        unsafe {
            let result = task_suspend(self.task);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::AttachFailed(format!("task_suspend failed: {}", result)));
            }
        }

        self.stopped = true;
        self.stop_reason = StopReason::Suspended;
        Ok(())
    }

    fn resume(&mut self) -> Result<()>
    {
        self.ensure_attached()?;
        if !self.stopped {
            return Ok(());
        }

        unsafe {
            let result = task_resume(self.task);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::AttachFailed(format!("task_resume failed: {}", result)));
            }
        }

        self.stopped = false;
        self.stop_reason = StopReason::Running;
        Ok(())
    }

    fn threads(&self) -> Result<Vec<ThreadId>>
    {
        self.ensure_attached()?;
        Ok(self.threads.iter().copied().map(|t| ThreadId::from(t as u64)).collect())
    }

    fn active_thread(&self) -> Option<ThreadId>
    {
        self.current_thread.map(|t| ThreadId::from(t as u64))
    }

    fn set_active_thread(&mut self, thread: ThreadId) -> Result<()>
    {
        let port = thread
            .raw()
            .try_into()
            .map_err(|_| DebuggerError::InvalidArgument(format!("Invalid thread id {}", thread.raw())))?;
        self.set_active_thread_by_port(port)
    }

    fn refresh_threads(&mut self) -> Result<()>
    {
        self.refresh_thread_list()
    }
}
