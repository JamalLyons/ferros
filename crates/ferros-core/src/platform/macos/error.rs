//! # macOS Mach API Errors
//!
//! Error types for macOS Mach API operations.
//!
//! Mach APIs return `kern_return_t` values, which are integer error codes.
//! This module converts those codes into Rust error types with descriptive
//! messages.

use thiserror::Error;

/// Mach kernel API error
///
/// Mach APIs return `kern_return_t` values. This enum covers the most common
/// error codes encountered during debugging operations.
///
/// ## Common Error Codes
///
/// - `KERN_SUCCESS` (0): Operation succeeded
/// - `KERN_INVALID_ADDRESS` (1): Invalid memory address
/// - `KERN_PROTECTION_FAILURE` (2): Memory protection violation
/// - `KERN_NO_SPACE` (3): No space available
/// - `KERN_INVALID_ARGUMENT` (4): Invalid argument
/// - `KERN_FAILURE` (5): General failure
/// - `KERN_RESOURCE_SHORTAGE` (6): Resource shortage
/// - `KERN_INVALID_TASK` (16): Invalid task port
/// - `KERN_INVALID_RIGHT` (17): Invalid port right
/// - `KERN_TERMINATED` (37): Task/thread has been terminated
/// - `KERN_NOT_SUPPORTED` (46): Operation not supported
///
/// ## References
///
/// - [kern_return_t documentation](https://developer.apple.com/documentation/kernel/kern_return_t)
/// - [Mach Error Codes](https://opensource.apple.com/source/xnu/xnu-7195.141.2/osfmk/mach/kern_return.h.auto.html)
#[derive(Error, Debug)]
pub enum MachError
{
    /// `KERN_INVALID_ADDRESS` (error code 1)
    ///
    /// The specified memory address is invalid or not accessible.
    /// Common causes:
    /// - Address is outside the process's address space
    /// - Address is in unmapped memory
    /// - Address is in kernel space (not accessible from user space)
    ///
    /// See: [vm_read(3) man page](https://developer.apple.com/documentation/kernel/1585350-vm_read/)
    #[error("KERN_INVALID_ADDRESS: Invalid memory address")]
    InvalidAddress,

    /// `KERN_PROTECTION_FAILURE` (error code 2)
    ///
    /// The operation was blocked due to memory protection or security restrictions.
    /// Common causes:
    /// - Attempted to read/write memory without proper permissions
    /// - `task_for_pid()` requires special permissions (need sudo or debugging entitlements)
    /// - System Integrity Protection (SIP) is blocking the operation
    /// - Memory region is read-only but write was attempted
    ///
    /// See: [macOS Debugging Entitlements](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)
    #[error("KERN_PROTECTION_FAILURE: Permission denied or memory protection violation")]
    ProtectionFailure,

    /// `KERN_NO_SPACE` (error code 3)
    ///
    /// Insufficient space to complete the operation.
    /// Common causes:
    /// - No memory region found at the specified address
    /// - Address space is exhausted
    /// - Buffer is too small
    ///
    /// See: [mach_vm_region(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_region/)
    #[error("KERN_NO_SPACE: No space available or region not found")]
    NoSpace,

    /// `KERN_INVALID_ARGUMENT` (error code 4)
    ///
    /// One of the arguments passed to the Mach API was invalid.
    /// Examples:
    /// - Invalid PID (process doesn't exist)
    /// - Invalid thread state flavor (wrong architecture)
    /// - Invalid thread port
    /// - Invalid memory size
    ///
    /// See: [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
    #[error("KERN_INVALID_ARGUMENT: Invalid argument provided")]
    InvalidArgument,

    /// `KERN_FAILURE` (error code 5)
    ///
    /// A general failure occurred. This is a catch-all error code.
    /// Can mean:
    /// - Process not found (if process doesn't exist)
    /// - Permission denied (macOS quirk: sometimes returns KERN_FAILURE instead of KERN_PROTECTION_FAILURE)
    /// - Thread has exited
    /// - Task port is invalid
    /// - Operation failed for an unspecified reason
    ///
    /// **Note**: The `attach()` function checks if the process exists when it receives
    /// `KERN_FAILURE`. If the process exists, it converts this to a `PermissionDenied` error
    /// instead, providing clearer error messages.
    #[error("KERN_FAILURE: General failure")]
    Failure,

    /// `KERN_RESOURCE_SHORTAGE` (error code 6)
    ///
    /// Insufficient system resources to complete the operation.
    /// Common causes:
    /// - Too many open file descriptors
    /// - Memory allocation failed
    /// - System is under heavy load
    #[error("KERN_RESOURCE_SHORTAGE: Insufficient system resources")]
    ResourceShortage,

    /// `KERN_INVALID_TASK` (error code 16)
    ///
    /// The task port is invalid or has been deallocated.
    /// Common causes:
    /// - Task port was deallocated
    /// - Process has exited
    /// - Task port was never valid
    ///
    /// See: [mach_port_deallocate(3) man page](https://developer.apple.com/documentation/kernel/1578777-mach_port_deallocate/)
    #[error("KERN_INVALID_TASK: Invalid task port")]
    InvalidTask,

    /// `KERN_INVALID_RIGHT` (error code 17)
    ///
    /// The Mach port right is invalid or has been deallocated.
    /// Common causes:
    /// - Port was deallocated
    /// - Port right doesn't exist
    /// - Attempted to use a port after it was deallocated
    ///
    /// See: [mach_port_deallocate(3) man page](https://developer.apple.com/documentation/kernel/1578777-mach_port_deallocate/)
    #[error("KERN_INVALID_RIGHT: Invalid port right")]
    InvalidRight,

    /// `KERN_TERMINATED` (error code 37)
    ///
    /// The task or thread has been terminated.
    /// Common causes:
    /// - Process exited
    /// - Thread was killed
    /// - Task was terminated
    #[error("KERN_TERMINATED: Task or thread has been terminated")]
    Terminated,

    /// `KERN_NOT_SUPPORTED` (error code 46)
    ///
    /// The operation is not supported on this system or architecture.
    /// Common causes:
    /// - Operation not available on this macOS version
    /// - Architecture doesn't support the operation
    /// - Feature is disabled
    #[error("KERN_NOT_SUPPORTED: Operation not supported")]
    NotSupported,

    /// Unknown Mach error code
    ///
    /// We received an error code we don't recognize. This could be:
    /// - A new error code in a newer macOS version
    /// - An error code specific to a particular operation
    /// - A corrupted return value
    ///
    /// The integer value is preserved so you can look it up.
    #[error("Unknown Mach error: {0}")]
    Unknown(i32),
}

/// Convert a `kern_return_t` to a `MachError`
///
/// This allows us to use `?` operator with Mach API calls:
///
/// ```rust,no_run
/// use ferros_core::platform::macos::error::MachError;
/// use libc::{c_int, mach_port_t};
/// use mach2::kern_return::KERN_SUCCESS;
///
/// unsafe extern "C" {
///     fn task_for_pid(
///         target_task: mach_port_t,
///         pid: c_int,
///         task: *mut mach_port_t,
///     ) -> libc::kern_return_t;
/// }
///
/// # let target_task = unsafe { mach2::traps::mach_task_self() };
/// # let pid = 12345;
/// # let mut task: mach_port_t = 0;
/// let result: Result<(), MachError> = unsafe {
///     let kr = task_for_pid(target_task, pid, &mut task);
///     if kr != KERN_SUCCESS {
///         return Err(MachError::from(kr));
///     }
///     Ok(())
/// };
/// # Ok::<(), MachError>(())
/// ```
///
/// ## Mach Error Constants
///
/// These constants are available in both `libc` and `mach2`:
/// - `KERN_SUCCESS` = 0 (we use `mach2::kern_return::KERN_SUCCESS`)
/// - `KERN_INVALID_ADDRESS` = 1 (from `libc`)
/// - `KERN_PROTECTION_FAILURE` = 2 (from `libc`)
/// - `KERN_NO_SPACE` = 3 (from `libc`)
/// - `KERN_INVALID_ARGUMENT` = 4 (from `libc`)
/// - `KERN_FAILURE` = 5 (from `libc`)
/// - `KERN_RESOURCE_SHORTAGE` = 6 (from `libc`)
/// - `KERN_INVALID_TASK` = 16 (from `libc`)
/// - `KERN_INVALID_RIGHT` = 17 (from `libc`)
/// - `KERN_TERMINATED` = 37 (from `libc`)
/// - `KERN_NOT_SUPPORTED` = 46 (from `libc`)
///
/// We use `libc` constants here for compatibility, but `mach2`'s constants
/// are equivalent and better maintained.
impl From<libc::kern_return_t> for MachError
{
    fn from(code: libc::kern_return_t) -> Self
    {
        match code {
            libc::KERN_INVALID_ADDRESS => MachError::InvalidAddress,
            libc::KERN_PROTECTION_FAILURE => MachError::ProtectionFailure,
            libc::KERN_NO_SPACE => MachError::NoSpace,
            libc::KERN_INVALID_ARGUMENT => MachError::InvalidArgument,
            libc::KERN_FAILURE => MachError::Failure,
            libc::KERN_RESOURCE_SHORTAGE => MachError::ResourceShortage,
            libc::KERN_INVALID_TASK => MachError::InvalidTask,
            libc::KERN_INVALID_RIGHT => MachError::InvalidRight,
            libc::KERN_TERMINATED => MachError::Terminated,
            libc::KERN_NOT_SUPPORTED => MachError::NotSupported,
            _ => MachError::Unknown(code),
        }
    }
}
