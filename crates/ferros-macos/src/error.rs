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
    /// - `task_for_pid()` requires the debugger entitlement
    ///   (`com.apple.security.cs.debugger`). Sign with `just sign-debugger`
    /// - Root also satisfies the check. Do not use `sudo` for tests
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
/// use ferros_macos::error::MachError;
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

/// Convert a `MachError` into the portable `DebuggerError`
///
/// `From<kern_return_t> for MachError` keeps the raw code only for `Unknown`, so the
/// mapping is:
///
/// - `ProtectionFailure` -> `DebuggerError::PermissionDenied`
/// - `InvalidArgument` -> `DebuggerError::Os` with code `KERN_INVALID_ARGUMENT`
/// - `ProcessNotFound` (`KERN_FAILURE`) -> `DebuggerError::Os` with code `KERN_FAILURE`
/// - `Unknown(code)` -> `DebuggerError::Os` with that code
///
/// Every variant carries the `MachError` display text as its message. A
/// `KERN_INVALID_ARGUMENT` from a Mach call is an OS failure (for example a thread port
/// whose thread has exited), not a bad argument from the debugger's caller, so it keeps
/// its code instead of becoming `DebuggerError::InvalidArgument` (which the `Backend`
/// boundary reports as `Unsupported`). Code that still knows the operation uses
/// [`mach_failure`] instead, which also names it.
impl From<MachError> for ferros_core::DebuggerError
{
    fn from(err: MachError) -> Self
    {
        let message = err.to_string();
        match err {
            MachError::ProtectionFailure => ferros_core::DebuggerError::PermissionDenied(message),
            MachError::InvalidArgument => ferros_core::DebuggerError::Os {
                code: i64::from(libc::KERN_INVALID_ARGUMENT),
                message,
            },
            MachError::ProcessNotFound => ferros_core::DebuggerError::Os {
                code: i64::from(libc::KERN_FAILURE),
                message,
            },
            MachError::Unknown(code) => ferros_core::DebuggerError::Os {
                code: i64::from(code),
                message,
            },
        }
    }
}

/// Classify a failed Mach call of `operation` while its `kern_return_t` is still known.
///
/// - `KERN_INVALID_ADDRESS` -> [`BackendError::NotFound`]
/// - `KERN_PROTECTION_FAILURE` -> [`BackendError::PermissionDenied`]
/// - any other code -> [`BackendError::Os`] with that code
///
/// Every message names the operation and the raw code, so no category loses it.
///
/// [`BackendError::NotFound`]: ferros_core::BackendError::NotFound
/// [`BackendError::PermissionDenied`]: ferros_core::BackendError::PermissionDenied
/// [`BackendError::Os`]: ferros_core::BackendError::Os
pub(crate) fn mach_failure(operation: impl std::fmt::Display, code: libc::kern_return_t) -> ferros_core::BackendError
{
    use ferros_core::BackendError;

    let message = format!("{operation}: {} (kern_return_t {code})", MachError::from(code));
    match code {
        libc::KERN_INVALID_ADDRESS => BackendError::NotFound(message),
        libc::KERN_PROTECTION_FAILURE => BackendError::PermissionDenied(message),
        _ => BackendError::Os {
            code: i64::from(code),
            message,
        },
    }
}

/// `error` with its message replaced by `reword(message)`, in the same category.
pub(crate) fn reword(error: ferros_core::BackendError, reword: impl FnOnce(&str) -> String) -> ferros_core::BackendError
{
    use ferros_core::BackendError;

    match error {
        BackendError::NotFound(m) => BackendError::NotFound(reword(&m)),
        BackendError::PermissionDenied(m) => BackendError::PermissionDenied(reword(&m)),
        BackendError::InvalidState(m) => BackendError::InvalidState(reword(&m)),
        BackendError::Unsupported(m) => BackendError::Unsupported(reword(&m)),
        BackendError::DetachCleanup(m) => BackendError::DetachCleanup(reword(&m)),
        BackendError::KillCleanup(m) => BackendError::KillCleanup(reword(&m)),
        BackendError::Os { code, message } => BackendError::Os {
            code,
            message: reword(&message),
        },
        BackendError::StopPending => BackendError::StopPending,
    }
}

/// Classify a failed VM read while its `kern_return_t` and address are still known.
///
/// - `KERN_INVALID_ADDRESS` (the range is not mapped) -> [`BackendError::NotFound`]
/// - `KERN_PROTECTION_FAILURE` (mapped but not readable) -> [`BackendError::PermissionDenied`]
/// - any other code -> [`BackendError::Os`] with that code, the operation, and the address
///
/// The code is never formatted into text first, so a caller sees the same category
/// whichever read path (cached or uncached) met the failure.
///
/// [`BackendError::NotFound`]: ferros_core::BackendError::NotFound
/// [`BackendError::PermissionDenied`]: ferros_core::BackendError::PermissionDenied
/// [`BackendError::Os`]: ferros_core::BackendError::Os
pub(crate) fn vm_read_error(
    operation: &'static str,
    at: ferros_core::Address,
    code: libc::kern_return_t,
) -> ferros_core::BackendError
{
    use ferros_core::BackendError;

    match code {
        libc::KERN_INVALID_ADDRESS => BackendError::NotFound(format!("{operation}: address {at} is not mapped")),
        libc::KERN_PROTECTION_FAILURE => {
            BackendError::PermissionDenied(format!("{operation}: memory at {at} is not readable"))
        }
        _ => BackendError::Os {
            code: i64::from(code),
            message: format!("{operation} at {at}: {}", MachError::from(code)),
        },
    }
}

#[cfg(test)]
mod tests
{
    use ferros_core::{Address, BackendError, DebuggerError};

    use super::{MachError, mach_failure, vm_read_error};

    #[test]
    fn protection_failure_maps_to_permission_denied()
    {
        let err = DebuggerError::from(MachError::from(libc::KERN_PROTECTION_FAILURE));
        match err {
            DebuggerError::PermissionDenied(message) => {
                assert_eq!(message, MachError::ProtectionFailure.to_string());
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[test]
    fn invalid_argument_maps_to_os_with_kern_invalid_argument_code()
    {
        let err = DebuggerError::from(MachError::from(libc::KERN_INVALID_ARGUMENT));
        match err {
            DebuggerError::Os { code, message } => {
                assert_eq!(code, i64::from(libc::KERN_INVALID_ARGUMENT));
                assert_eq!(message, MachError::InvalidArgument.to_string());
            }
            other => panic!("expected Os, got {other:?}"),
        }
    }

    #[test]
    fn kern_failure_maps_to_os_with_kern_failure_code()
    {
        let err = DebuggerError::from(MachError::from(libc::KERN_FAILURE));
        match err {
            DebuggerError::Os { code, message } => {
                assert_eq!(code, i64::from(libc::KERN_FAILURE));
                assert_eq!(message, MachError::ProcessNotFound.to_string());
            }
            other => panic!("expected Os, got {other:?}"),
        }
    }

    #[test]
    fn unknown_code_maps_to_os_with_the_raw_code()
    {
        let kr = libc::KERN_INVALID_ADDRESS;
        let err = DebuggerError::from(MachError::from(kr));
        match err {
            DebuggerError::Os { code, message } => {
                assert_eq!(code, i64::from(kr));
                assert_eq!(message, MachError::Unknown(kr).to_string());
            }
            other => panic!("expected Os, got {other:?}"),
        }
    }

    #[test]
    fn vm_read_of_an_unmapped_address_is_not_found()
    {
        let err = vm_read_error("mach_vm_read_overwrite", Address::from(0x10u64), libc::KERN_INVALID_ADDRESS);
        assert!(
            matches!(err, BackendError::NotFound(ref m) if m.contains("0x0000000000000010")),
            "{err:?}"
        );
    }

    #[test]
    fn vm_read_of_a_protected_address_is_permission_denied()
    {
        let err = vm_read_error(
            "mach_vm_read_overwrite",
            Address::from(0x10u64),
            libc::KERN_PROTECTION_FAILURE,
        );
        assert!(matches!(err, BackendError::PermissionDenied(_)), "{err:?}");
    }

    #[test]
    fn vm_read_of_another_failure_keeps_the_code_and_address()
    {
        let err = vm_read_error("mach_vm_read_overwrite", Address::from(0x10u64), libc::KERN_MEMORY_ERROR);
        match err {
            BackendError::Os { code, message } => {
                assert_eq!(code, i64::from(libc::KERN_MEMORY_ERROR));
                assert!(message.contains("0x0000000000000010"), "{message}");
            }
            other => panic!("expected Os, got {other:?}"),
        }
    }

    #[test]
    fn mach_failure_keeps_the_code_in_every_category()
    {
        let err = mach_failure("thread_get_state", mach2::message::MACH_SEND_INVALID_DEST);
        assert_eq!(
            err,
            BackendError::Os {
                code: i64::from(mach2::message::MACH_SEND_INVALID_DEST),
                message: format!(
                    "thread_get_state: {} (kern_return_t {})",
                    MachError::Unknown(mach2::message::MACH_SEND_INVALID_DEST),
                    mach2::message::MACH_SEND_INVALID_DEST
                ),
            }
        );
        let err = mach_failure("mach_vm_protect", libc::KERN_PROTECTION_FAILURE);
        assert!(
            matches!(err, BackendError::PermissionDenied(ref m) if m.contains("kern_return_t 2")),
            "{err:?}"
        );
        let err = mach_failure("thread_get_state", libc::KERN_INVALID_ARGUMENT);
        assert!(
            matches!(err, BackendError::Os { code, ref message }
                if code == i64::from(libc::KERN_INVALID_ARGUMENT) && message.contains("kern_return_t 4")),
            "{err:?}"
        );
        let err = mach_failure("vm_write", libc::KERN_INVALID_ADDRESS);
        assert!(
            matches!(err, BackendError::NotFound(ref m) if m.contains("kern_return_t 1")),
            "{err:?}"
        );
    }
}
