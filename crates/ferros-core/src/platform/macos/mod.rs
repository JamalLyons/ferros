//! # macOS Debugging Implementation
//!
//! macOS-specific debugger implementation using Mach APIs.
//!
//! macOS uses the Mach microkernel, which provides debugging capabilities
//! through Mach ports and messages. Unlike Linux's `ptrace`, macOS debugging
//! is based on:
//!
//! - **Mach tasks**: Represent a process
//! - **Mach threads**: Represent threads within a task
//! - **Mach ports**: Communication channels to tasks/threads
//!
//! ## Platform Requirements
//!
//! ### Minimum macOS Version
//!
//! - **macOS 10.5 (Leopard) or later**: Required for `POSIX_SPAWN_START_SUSPENDED` flag
//!   used in `launch()`. This flag allows spawning processes in a suspended state.
//! - **Recommended: macOS 10.15 (Catalina) or later**: For best compatibility with
//!   modern Rust toolchains and development tools.
//!
//! ### Architecture Support
//!
//! - **ARM64 (Apple Silicon)**: Fully supported (M1, M2, M3, M4, etc.)
//!   - Primary target architecture
//!   - Uses `ARM_THREAD_STATE64` flavor for register access
//! - **x86_64 (Intel Mac)**: Supported for compatibility
//!   - Uses `X86_THREAD_STATE64` flavor for register access
//!   - Note: Intel Macs are no longer sold, but support is maintained
//!
//! ## Key Mach APIs Used
//!
//! - `task_for_pid()`: Get a Mach port to a process (declared ourselves - not in mach2)
//! - `task_threads()`: Enumerate threads in a task (from `mach2` crate)
//! - `thread_get_state()`: Read thread registers (declared ourselves - not in mach2)
//! - `thread_set_state()`: Write thread registers (future)
//! - `posix_spawn()`: Launch processes with `POSIX_SPAWN_START_SUSPENDED` flag
//!
//! ## Dependencies
//!
//! We use a hybrid approach:
//! - **mach2 crate**: For well-maintained Mach APIs (`mach_task_self`, `task_threads`, `KERN_SUCCESS`)
//! - **libc crate**: For type definitions (`mach_port_t`, `thread_act_t`, `posix_spawnattr_t`, etc.)
//! - **ffi module**: Centralized FFI declarations for restricted functions not in mach2
//!   (`task_for_pid`, `thread_get_state`, `vm_read`, `vm_write`, `mach_vm_region`, `posix_spawn`)
//!
//! This gives us the best of both worlds: well-maintained APIs where available,
//! and direct control over restricted functions.
//!
//! ## References
//!
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - [XNU Kernel Source](https://github.com/apple-oss-distributions/xnu) (for `task_for_pid` implementation)
//! - [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
//! - [posix_spawn documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawn.3.html)
//! - [macOS Debugging Entitlements](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)

use ferros_utils::debug;
use libc::{c_int, mach_port_t};
use mach2::kern_return::KERN_SUCCESS;
use mach2::task::task_threads;
use mach2::traps::mach_task_self;

use crate::error::{FerrosError, FerrosResult};
use crate::types::process::ProcessId;

pub mod error;
pub mod ffi;

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
#[derive(Debug)]
pub struct MacOSDebugger
{
    /// Mach task port for the attached process
    ///
    /// This is `None` when not attached to any process. When attached,
    /// this contains the task port obtained from `task_for_pid()`.
    pub task_port: Option<mach_port_t>,
    /// Process ID of the attached process
    ///
    /// This is `None` when not attached to any process. When attached,
    /// this contains the PID that was used to obtain the task port.
    pub pid: Option<u32>,
}

impl MacOSDebugger
{
    /// Create a new macOS debugger
    ///
    /// This function creates a new macOS debugger instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the debugger cannot be created.
    pub fn new() -> FerrosResult<Self>
    {
        debug!("New MacOS debugger created");
        Ok(Self {
            task_port: None,
            pid: None,
        })
    }
}

impl Drop for MacOSDebugger
{
    fn drop(&mut self)
    {
        // Clean up the task port if we're still attached
        if let Some(task_port) = self.task_port {
            let self_task = unsafe { mach_task_self() };
            let result = unsafe { ffi::mach_port_deallocate(self_task, task_port) };
            if result != KERN_SUCCESS {
                debug!("Failed to deallocate task port in Drop: {}", result);
            }
        }
    }
}

impl crate::debugger::FerrosDebugger for MacOSDebugger
{
    fn launch(&mut self, _program: &str, _args: &[&str]) -> FerrosResult<crate::types::process::ProcessId>
    {
        Err(crate::error::FerrosError::AttachFailed(
            "launch() not yet implemented".to_string(),
        ))
    }

    fn attach(&mut self, pid: ProcessId) -> FerrosResult<()>
    {
        let pid_value: u32 = pid.into();
        debug!("Attaching to process {}", pid_value);

        // Check if we're already attached
        if self.task_port.is_some() {
            return Err(FerrosError::AttachFailed(
                "Already attached to a process. Call detach() first.".to_string(),
            ));
        }

        // Verify the process exists by sending signal 0 (no-op signal)
        // This is a standard way to check if a process exists without affecting it
        let process_exists = unsafe { libc::kill(pid_value as libc::pid_t, 0) == 0 };
        if !process_exists {
            return Err(FerrosError::ProcessNotFound(pid_value));
        }

        // Get our own task port
        let self_task = unsafe { mach_task_self() };

        // Get the task port for the target process
        let mut task_port: mach_port_t = 0;
        let result = unsafe { ffi::task_for_pid(self_task, pid_value as c_int, &mut task_port) };

        // Convert Mach error to our error type
        if result != KERN_SUCCESS {
            let mach_error = error::MachError::from(result);
            match mach_error {
                error::MachError::ProtectionFailure => {
                    return Err(FerrosError::PermissionDenied(format!(
                        "task_for_pid() failed: {}. Need sudo or debugging entitlements.",
                        mach_error
                    )));
                }
                error::MachError::InvalidArgument => {
                    // If we got here, the process exists (we checked above),
                    // so this is likely a permission issue
                    return Err(FerrosError::PermissionDenied(format!(
                        "task_for_pid() failed: {}. Need sudo or debugging entitlements.",
                        mach_error
                    )));
                }
                error::MachError::ProcessNotFound => {
                    // Even though we checked above, the process might have exited
                    // between the check and the attach call
                    return Err(FerrosError::ProcessNotFound(pid_value));
                }
                error::MachError::Unknown(code) => {
                    return Err(FerrosError::AttachFailed(format!(
                        "task_for_pid() failed with unknown error code: {}",
                        code
                    )));
                }
            }
        }

        // Verify the attachment by enumerating threads
        // This ensures the task port is valid and we can actually interact with the process
        let mut thread_list: *mut libc::thread_act_t = std::ptr::null_mut();
        let mut thread_count: libc::mach_msg_type_number_t = 0;
        let thread_result = unsafe { task_threads(task_port, &mut thread_list, &mut thread_count) };

        if thread_result != KERN_SUCCESS {
            // Clean up the task port we just obtained
            let dealloc_result = unsafe { ffi::mach_port_deallocate(self_task, task_port) };
            if dealloc_result != KERN_SUCCESS {
                debug!("Failed to deallocate task port after thread enumeration failure");
            }

            return Err(FerrosError::AttachFailed(format!(
                "Failed to enumerate threads: {}",
                error::MachError::from(thread_result)
            )));
        }

        // Deallocate the thread list (task_threads allocates memory that must be freed)
        if !thread_list.is_null() && thread_count > 0 {
            unsafe {
                let vm_result = ffi::vm_deallocate(
                    self_task,
                    thread_list as libc::vm_address_t,
                    (thread_count as usize * std::mem::size_of::<libc::thread_act_t>()) as libc::vm_size_t,
                );
                if vm_result != KERN_SUCCESS {
                    debug!("Failed to deallocate thread list: {}", vm_result);
                }
            }
        }

        // Store the task port and PID
        self.task_port = Some(task_port);
        self.pid = Some(pid_value);

        debug!("Successfully attached to process {} (task port: {})", pid_value, task_port);
        Ok(())
    }

    fn detach(&mut self) -> FerrosResult<()>
    {
        if let Some(task_port) = self.task_port.take() {
            let self_task = unsafe { mach_task_self() };
            let result = unsafe { ffi::mach_port_deallocate(self_task, task_port) };
            if result != KERN_SUCCESS {
                return Err(FerrosError::AttachFailed(format!(
                    "Failed to deallocate task port: {}",
                    error::MachError::from(result)
                )));
            }
            debug!("Detached from process {}", self.pid.unwrap_or(0));
            self.pid = None;
        }
        Ok(())
    }
}
