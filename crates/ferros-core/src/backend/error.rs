//! Portable error type returned by every [`Backend`](super::Backend) method.

use thiserror::Error;

/// Error returned by a [`Backend`](super::Backend) or an [`Arch`](crate::arch::Arch).
///
/// The variants are deliberately few and portable: every operating system's failure
/// is mapped onto one of them by the backend, so engine code never sees an OS type.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum BackendError
{
    /// The process, thread, or program does not exist, or the process has exited.
    #[error("not found: {0}")]
    NotFound(String),

    /// The operating system refused the operation for lack of privileges.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The call is illegal in the backend's current state (for example reading
    /// registers while the target is running, or any target call with no process).
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// The backend or architecture does not support the requested operation.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Any other operating-system failure, carrying the raw OS code and message.
    #[error("OS error {code}: {message}")]
    Os
    {
        /// Raw error code reported by the operating system (errno, kern_return_t, ...).
        code: i64,
        /// Human-readable description of the failure.
        message: String,
    },
}

/// Result type of [`Backend`](super::Backend) and [`Arch`](crate::arch::Arch) operations.
pub type BackendResult<T> = std::result::Result<T, BackendError>;
