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

use std::ffi::CString;

use ferros_utils::debug;
use libc::{c_int, mach_port_t, pid_t, thread_act_t};
use mach2::kern_return::KERN_SUCCESS;
use mach2::task::task_threads;
use mach2::traps::mach_task_self;

use crate::error::{FerrosError, FerrosResult};
use crate::types::process::{Architecture, ProcessId};

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
    /// Mach task port for the attached process.
    ///
    /// This is `None` when not attached to any process. When attached,
    /// this contains the task port obtained from `task_for_pid()`.
    pub task_port: Option<mach_port_t>,
    /// Process ID of the attached process.
    ///
    /// This is `None` when not attached to any process. When attached,
    /// this contains the PID that was used to obtain the task port.
    pub pid: Option<u32>,
    /// Architecture metadata.
    pub architecture: Architecture,
    /// Cached thread ports for the target task.
    pub threads: Vec<thread_act_t>,
    /// Active thread used for register operations.
    pub current_thread: Option<thread_act_t>,
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
            architecture: Architecture::current(),
            threads: Vec::new(),
            current_thread: None,
        })
    }
}

impl Drop for MacOSDebugger
{
    fn drop(&mut self)
    {
        let self_task = unsafe { mach_task_self() };

        // Clean up thread ports
        for thread_port in &self.threads {
            let result = unsafe { ffi::mach_port_deallocate(self_task, *thread_port) };
            if result != KERN_SUCCESS {
                debug!("Failed to deallocate thread port in Drop: {}", result);
            }
        }
        self.threads.clear();
        self.current_thread = None;

        // Clean up the task port if we're still attached
        if let Some(task_port) = self.task_port {
            let result = unsafe { ffi::mach_port_deallocate(self_task, task_port) };
            if result != KERN_SUCCESS {
                debug!("Failed to deallocate task port in Drop: {}", result);
            }
        }
    }
}

impl crate::debugger::FerrosDebugger for MacOSDebugger
{
    fn launch(&mut self, program: &str, args: &[&str]) -> FerrosResult<crate::types::process::ProcessId>
    {
        // Cannot launch if already attached
        if self.task_port.is_some() {
            return Err(FerrosError::AttachFailed(
                "Already attached to a process. Call detach() first.".to_string(),
            ));
        }

        // Build argv: program path followed by args, null-terminated
        let mut argv_cstrings = Vec::with_capacity(args.len() + 1);
        argv_cstrings
            .push(CString::new(program).map_err(|e| FerrosError::InvalidArgument(format!("Invalid program path: {}", e)))?);
        for arg in args {
            argv_cstrings
                .push(CString::new(*arg).map_err(|e| FerrosError::InvalidArgument(format!("Invalid argument: {}", e)))?);
        }
        let mut argv_ptrs: Vec<*const libc::c_char> = argv_cstrings.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        // Initialize spawn attributes
        let mut attr: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
        let init_result = unsafe { ffi::posix_spawnattr_init(&mut attr as *mut _) };
        if init_result != 0 {
            return Err(FerrosError::AttachFailed(format!(
                "posix_spawnattr_init failed with errno {}",
                init_result
            )));
        }

        // Ensure we destroy the attr even on early returns
        struct AttrGuard(libc::posix_spawnattr_t);
        impl Drop for AttrGuard
        {
            fn drop(&mut self)
            {
                unsafe {
                    let _ = ffi::posix_spawnattr_destroy(&mut self.0 as *mut _);
                }
            }
        }
        let mut attr_guard = AttrGuard(attr);

        // Start the process suspended so the debugger controls initial execution
        let flag_result = unsafe {
            ffi::posix_spawnattr_setflags(&mut attr_guard.0 as *mut _, ffi::spawn_flags::POSIX_SPAWN_START_SUSPENDED)
        };
        if flag_result != 0 {
            return Err(FerrosError::AttachFailed(format!(
                "posix_spawnattr_setflags failed with errno {}",
                flag_result
            )));
        }

        // Spawn the process
        let mut child_pid: pid_t = 0;
        let spawn_result = unsafe {
            ffi::posix_spawn(
                &mut child_pid as *mut _,
                argv_cstrings[0].as_ptr(),
                std::ptr::null(),
                &attr_guard.0 as *const _,
                argv_ptrs.as_ptr(),
                std::ptr::null(),
            )
        };

        if spawn_result != 0 {
            return Err(FerrosError::AttachFailed(format!(
                "posix_spawn failed with errno {}",
                spawn_result
            )));
        }

        // Attach to the newly spawned (suspended) process
        let pid_value = child_pid as u32;
        self.attach(ProcessId::from(pid_value))?;

        Ok(ProcessId::from(pid_value))
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
                error::MachError::Failure => {
                    // KERN_FAILURE can mean either process not found OR permission denied.
                    // Since we already verified the process exists above, this is likely
                    // a permission issue. However, the process might have exited between
                    // the check and the attach call, so we verify again.
                    let still_exists = unsafe { libc::kill(pid_value as libc::pid_t, 0) == 0 };
                    if still_exists {
                        // Process still exists, so this is a permission issue
                        return Err(FerrosError::PermissionDenied(format!(
                            "task_for_pid() failed: {}. Process exists but access denied. Need sudo or debugging \
                             entitlements.",
                            mach_error
                        )));
                    } else {
                        // Process exited between check and attach
                        return Err(FerrosError::ProcessNotFound(pid_value));
                    }
                }
                error::MachError::InvalidTask => {
                    return Err(FerrosError::AttachFailed(format!(
                        "task_for_pid() failed: {}. Invalid task port.",
                        mach_error
                    )));
                }
                error::MachError::InvalidRight => {
                    return Err(FerrosError::AttachFailed(format!(
                        "task_for_pid() failed: {}. Invalid port right.",
                        mach_error
                    )));
                }
                error::MachError::Terminated => {
                    return Err(FerrosError::ProcessNotFound(pid_value));
                }
                error::MachError::InvalidAddress
                | error::MachError::NoSpace
                | error::MachError::ResourceShortage
                | error::MachError::NotSupported => {
                    return Err(FerrosError::AttachFailed(format!("task_for_pid() failed: {}", mach_error)));
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

        // Copy thread ports from the array into our Vec before deallocating the memory
        let mut threads = Vec::new();
        if !thread_list.is_null() && thread_count > 0 {
            let thread_slice = unsafe { std::slice::from_raw_parts(thread_list, thread_count as usize) };
            threads.extend_from_slice(thread_slice);
        }

        // Set the current thread to the first thread (if any)
        let current_thread = threads.first().copied();

        // Deallocate the thread list memory (task_threads allocates memory that must be freed)
        // Note: We've copied the thread ports, so we can safely deallocate the array memory
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

        // Store the task port, PID, threads, and current thread
        self.task_port = Some(task_port);
        self.pid = Some(pid_value);
        self.threads = threads;
        self.current_thread = current_thread;

        debug!("Successfully attached to process {} (task port: {})", pid_value, task_port);
        Ok(())
    }

    fn detach(&mut self) -> FerrosResult<()>
    {
        let self_task = unsafe { mach_task_self() };

        // Clean up thread ports
        for thread_port in &self.threads {
            let result = unsafe { ffi::mach_port_deallocate(self_task, *thread_port) };
            if result != KERN_SUCCESS {
                debug!("Failed to deallocate thread port in detach: {}", result);
                // Continue cleaning up other resources even if one fails
            }
        }
        self.threads.clear();
        self.current_thread = None;
        self.task_port = None;
        self.pid = None;

        // Clean up the task port
        if let Some(task_port) = self.task_port.take() {
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
