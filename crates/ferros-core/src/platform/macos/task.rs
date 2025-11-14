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
//! - [task_for_pid(3) man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_for_pid.3.html)
//! - [task_threads(3) man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_threads.3.html)
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)

use libc::{c_int, mach_msg_type_number_t, mach_port_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::task::task_threads;
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;

use crate::debugger::Debugger;
use crate::error::{DebuggerError, Result};
use crate::platform::macos::registers::read_registers_arm64;
use crate::types::{ProcessId, Registers};

/// macOS debugger implementation using Mach APIs
///
/// This struct holds the state needed to debug a process on macOS:
///
/// - `task`: A Mach port to the target process
/// - `main_thread`: A Mach port to the main thread
/// - `pid`: The process ID we're debugging
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

    /// Mach port to the main thread of the target process
    ///
    /// This is obtained from `task_threads()`. We use it to read/write
    /// thread state. A value of 0 means we're not attached.
    ///
    /// See: [thread_act_t documentation](https://developer.apple.com/documentation/kernel/thread_act_t)
    main_thread: thread_act_t,

    /// Process ID of the target process
    ///
    /// Stored for reference and error messages. The actual debugging
    /// uses the `task` port, not the PID.
    pid: ProcessId,
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
            main_thread: 0,
            pid: ProcessId(0),
        })
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
    /// See: [task_for_pid(3) man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_for_pid.3.html)
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
    /// See: [task_threads(3) man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_threads.3.html)
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
        // Note: task_for_pid() is NOT in mach2 (likely because it's restricted/requires entitlements)
        // So we still declare it ourselves using extern "C"
        //
        // We still use libc for:
        // - Type definitions (mach_port_t, thread_act_t, etc.)
        // - General C library functions
        extern "C" {
            /// Get a Mach port to a process by PID
            ///
            /// This function is NOT provided by mach2 (likely because it requires special permissions).
            /// We declare it ourselves using extern "C".
            ///
            /// See: [task_for_pid(3)](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_for_pid.3.html)
            fn task_for_pid(target_task: mach_port_t, pid: c_int, task: *mut mach_port_t) -> libc::kern_return_t;
        }

        unsafe {
            // Step 1: Get a Mach port to the target process
            // mach_task_self() returns our own task port (from mach2, not deprecated)
            // task_for_pid() requires special permissions (sudo or debugging entitlements)
            //
            // See: [task_for_pid(3)](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_for_pid.3.html)
            let mut task: mach_port_t = 0;
            let result = task_for_pid(mach_task_self(), pid.0 as c_int, &mut task);

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
            // See: [task_threads(3)](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_threads.3.html)
            let mut threads: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;

            let result = task_threads(task, &mut threads, &mut thread_count);
            if result != KERN_SUCCESS || thread_count == 0 {
                return Err(DebuggerError::AttachFailed(format!("Failed to get threads: {}", result)));
            }

            // Step 3: Store the task port, main thread, and PID
            // We use the first thread as the "main thread"
            // Note: We don't free the threads array - it's managed by the kernel
            self.task = task;
            self.main_thread = *threads;
            self.pid = pid;

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
        // On macOS, we don't need explicit detach
        // The task port is released when the struct is dropped
        self.task = 0;
        self.main_thread = 0;
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
        if self.main_thread == 0 {
            return Err(DebuggerError::AttachFailed("Not attached to a process".to_string()));
        }

        #[cfg(target_arch = "aarch64")]
        return read_registers_arm64(self.main_thread);

        #[cfg(target_arch = "x86_64")]
        return Err(DebuggerError::InvalidArgument(
            "x86_64 support not yet implemented".to_string(),
        ));

        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        return Err(DebuggerError::InvalidArgument("Unsupported architecture".to_string()));
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
}
