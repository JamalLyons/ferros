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
/// Mach APIs return `kern_return_t` values. Common values:
///
/// - `KERN_SUCCESS` (0): Operation succeeded
/// - `KERN_PROTECTION_FAILURE` (5): Permission denied
/// - `KERN_INVALID_ARGUMENT` (4): Invalid argument
/// - `KERN_FAILURE` (14): General failure (often means process not found)
///
/// ## Why convert to an enum?
///
/// - **Type safety**: Can match on specific error types
/// - **Better error messages**: Descriptive strings instead of numbers
/// - **Error chaining**: Can convert to `DebuggerError` automatically
///
/// ## References
///
/// - [kern_return_t documentation](https://developer.apple.com/documentation/kernel/kern_return_t)
#[derive(Error, Debug)]
pub enum MachError
{
    /// `KERN_PROTECTION_FAILURE` (error code 5)
    ///
    /// This means the operation was blocked by macOS's security system.
    /// Common causes:
    /// - `task_for_pid()` requires special permissions
    /// - Need to run with `sudo` or grant debugging entitlements
    /// - System Integrity Protection (SIP) is blocking the operation
    ///
    /// See: [macOS Debugging Entitlements](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)
    #[error("KERN_PROTECTION_FAILURE: Permission denied")]
    ProtectionFailure,

    /// `KERN_INVALID_ARGUMENT` (error code 4)
    ///
    /// One of the arguments passed to the Mach API was invalid.
    /// Examples:
    /// - Invalid PID (process doesn't exist)
    /// - Invalid thread state flavor
    /// - Invalid memory address
    #[error("KERN_INVALID_ARGUMENT: Invalid PID or argument")]
    InvalidArgument,

    /// `KERN_FAILURE` (error code 14)
    ///
    /// A general failure occurred. Can mean:
    /// - Process not found (if process doesn't exist)
    /// - Permission denied (macOS quirk: sometimes returns KERN_FAILURE instead of KERN_PROTECTION_FAILURE)
    /// - Thread has exited
    /// - Task port is invalid
    ///
    /// **Note**: The `attach()` function checks if the process exists when it receives
    /// `KERN_FAILURE`. If the process exists, it converts this to a `PermissionDenied` error
    /// instead, providing clearer error messages.
    #[error("KERN_FAILURE: Process not found")]
    ProcessNotFound,

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
/// - `KERN_PROTECTION_FAILURE` = 5 (from `libc`)
/// - `KERN_INVALID_ARGUMENT` = 4 (from `libc`)
/// - `KERN_FAILURE` = 14 (from `libc`)
///
/// We use `libc` constants here for compatibility, but `mach2`'s constants
/// are equivalent and better maintained.
impl From<libc::kern_return_t> for MachError
{
    fn from(code: libc::kern_return_t) -> Self
    {
        match code {
            libc::KERN_PROTECTION_FAILURE => MachError::ProtectionFailure,
            libc::KERN_INVALID_ARGUMENT => MachError::InvalidArgument,
            libc::KERN_FAILURE => MachError::ProcessNotFound,
            _ => MachError::Unknown(code),
        }
    }
}
