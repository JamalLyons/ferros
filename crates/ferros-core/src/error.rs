//! # Error Types
//!
//! General error handling for the debugger.
//!
//! We use `thiserror` to automatically generate `Error` trait implementations
//! and nice error messages.

use thiserror::Error;

/// Main error type for debugger operations
///
/// This enum represents all the ways a debugger operation can fail.
/// Each variant corresponds to a specific error condition that can occur
/// when interacting with processes.
///
/// ## Error Categories
///
/// 1. **Process errors**: ProcessNotFound, AttachFailed, NotAttached
/// 2. **State errors**: NotStopped, SuspendFailed, ResumeFailed
/// 3. **Breakpoint errors**: NoBreakpoint, BreakpointIdNotFound
/// 4. **Permission errors**: PermissionDenied
/// 5. **Resource errors**: ResourceExhausted (hardware breakpoint/watchpoint limits)
/// 6. **Platform errors**: MachError (macOS-specific)
/// 7. **I/O errors**: Io (for file operations, etc.)
#[derive(Error, Debug)]
pub enum FerrosError
{
    /// The process with the given PID doesn't exist or has exited
    ///
    /// This happens when:
    /// - You provide an invalid PID
    /// - The process exited between when you got its PID and when you tried to attach
    /// - The PID was from a different system (if doing remote debugging)
    #[error("Process not found: PID {0}")]
    ProcessNotFound(u32),

    /// Insufficient permissions to debug the target process
    ///
    /// On macOS, this typically means:
    /// - `task_for_pid()` returned `KERN_PROTECTION_FAILURE`
    /// - You need to run with `sudo` or grant debugging entitlements
    ///
    /// See: [macOS Debugging Entitlements](https://developer.apple.com/documentation/bundleresources/entitlements/com_apple_security_cs_debugger)
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// Invalid argument passed to a debugger function
    ///
    /// Examples:
    /// - Trying to read registers before attaching
    /// - Invalid memory address
    /// - Unsupported architecture
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// Failed to attach to a process
    ///
    /// This is a general error for attachment failures that don't fit
    /// into more specific categories. The string contains details about
    /// what went wrong.
    ///
    /// For more specific errors, see:
    /// - `NotAttached`: Debugger is not attached to any process
    /// - `PermissionDenied`: Insufficient permissions to attach
    /// - `ProcessNotFound`: Process doesn't exist
    #[error("Failed to attach to process: {0}")]
    AttachFailed(String),

    /// Operation requires the debugger to be attached to a process
    ///
    /// This error occurs when trying to perform an operation (like reading
    /// registers or memory) without first attaching to a process.
    ///
    /// ## Solution
    ///
    /// Call `attach(pid)` before performing operations on the process.
    #[error("Not attached to a process")]
    NotAttached,

    /// Operation requires the process to be stopped
    ///
    /// This error occurs when trying to perform an operation that requires
    /// the process to be stopped (like reading registers safely) while the
    /// process is still running.
    ///
    /// ## Solution
    ///
    /// Call `suspend()` before performing operations that require the process
    /// to be stopped.
    #[error("Process must be stopped for this operation")]
    NotStopped,

    /// No breakpoint found at the specified address
    ///
    /// This error occurs when trying to remove, disable, or query a breakpoint
    /// that doesn't exist at the given address.
    #[error("No breakpoint at address 0x{0:016x}")]
    NoBreakpoint(u64),

    /// No breakpoint exists for the given identifier.
    #[error("No breakpoint with id {0}")]
    BreakpointIdNotFound(u64),

    /// A required resource has been exhausted
    ///
    /// This error occurs when attempting to use a resource that has reached its
    /// hardware or system-imposed limit. Common scenarios:
    ///
    /// - **Hardware breakpoints**: CPU debug registers are full
    ///   - x86-64: Maximum 4 hardware breakpoints (DR0-DR3)
    ///   - ARM64: Maximum 16 hardware breakpoints (DBGBVR0-15)
    /// - **Watchpoints**: Hardware watchpoint registers are full
    /// - **Threads**: Maximum number of threads reached (platform-dependent)
    ///
    /// ## Solution
    ///
    /// Remove some existing breakpoints/watchpoints before adding new ones, or
    /// use software breakpoints which don't have hardware limits.
    #[error("Resource exhausted: {0}")]
    ResourceExhausted(String),

    /// Failed to suspend the target process
    ///
    /// This error occurs when `suspend()` fails. This can happen if:
    /// - The process has already exited
    /// - The task port is invalid
    /// - Insufficient permissions
    #[error("Failed to suspend process: {0}")]
    SuspendFailed(String),

    /// Failed to resume the target process
    ///
    /// This error occurs when `resume()` fails. This can happen if:
    /// - The process has already exited
    /// - The task port is invalid
    /// - Insufficient permissions
    #[error("Failed to resume process: {0}")]
    ResumeFailed(String),

    /// Failed to read registers from the target process
    ///
    /// This can happen if:
    /// - The thread has exited
    /// - The thread state structure doesn't match what we expect
    /// - The architecture is different than expected
    #[error("Failed to read registers: {operation}")]
    ReadRegistersFailed
    {
        /// Description of the operation that failed
        operation: String,
        /// Thread ID if the operation was thread-specific
        thread_id: Option<crate::types::process::ThreadId>,
        /// Additional error details
        details: String,
    },

    /// macOS-specific Mach API error
    ///
    /// This wraps errors from the Mach kernel APIs. Common errors:
    /// - `KERN_PROTECTION_FAILURE`: Permission denied
    /// - `KERN_INVALID_ARGUMENT`: Invalid PID or argument
    /// - `KERN_FAILURE`: Process not found
    ///
    /// See: [Mach Kernel Return Codes](https://developer.apple.com/documentation/kernel/kern_return_t)
    #[cfg(target_os = "macos")]
    #[error("Mach API error: {0}")]
    MachError(#[from] crate::platform::macos::error::MachError),

    /// I/O error (for file operations, etc.)
    ///
    /// Used for errors when reading/writing files, sockets, etc.
    /// This is a standard Rust `std::io::Error` converted to our error type.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience type alias for `Result<T, DebuggerError>`
///
/// ```rust
/// use ferros_core::error::FerrosResult;
/// fn foo() -> FerrosResult<()>
/// {
///     Ok(())
/// }
/// ```
pub type FerrosResult<T> = std::result::Result<T, FerrosError>;
