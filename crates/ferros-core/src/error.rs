//! # Error Types
//!
//! Error handling for the debugger.
//!
//! We use `thiserror` to automatically generate `Error` trait implementations
//! and nice error messages. This makes error handling ergonomic while still
//! giving us full control over error types.

use thiserror::Error;

use crate::backend::BackendError;
use crate::symbols::SourceLookupError;

/// Main error type for debugger operations
///
/// This enum represents all the ways a debugger operation can fail.
/// Each variant corresponds to a specific error condition that can occur
/// when interacting with processes.
///
/// ## Why use an enum instead of String?
///
/// - **Type safety**: The compiler ensures we handle all error cases
/// - **Pattern matching**: Can match on specific error types
/// - **Better error messages**: Each variant can have a custom message
/// - **Error chaining**: Platform crates convert their own errors into these variants
///
/// ## Error Categories
///
/// 1. **Process errors**: ProcessNotFound, AttachFailed, NotAttached
/// 2. **State errors**: NotStopped, SuspendFailed, ResumeFailed
/// 3. **Breakpoint errors**: NoBreakpoint, BreakpointIdNotFound
/// 4. **Permission errors**: PermissionDenied
/// 5. **Resource errors**: ResourceExhausted (hardware breakpoint/watchpoint limits)
/// 6. **OS errors**: Os (any other operating-system failure, with the raw OS code)
/// 7. **I/O errors**: Io (for file operations, etc.)
#[derive(Error, Debug)]
pub enum DebuggerError
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
    /// On Linux, this means:
    /// - `ptrace(PTRACE_ATTACH)` failed with EPERM
    /// - You need to be root or the process owner
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

    /// No usable embedded or UUID-matched companion debug information was found.
    #[error(
        "No debug information for {path}: {detail}. Build with debug symbols and split-debuginfo = \"packed\", or \
         install/run dsymutil to generate a matching dSYM"
    )]
    NoDebugInfo
    {
        /// Executable whose symbols were requested.
        path: std::path::PathBuf,
        /// Discovery or generation failure.
        detail: String,
    },

    /// A loaded image's segment layout could not be read from the target, so its file
    /// addresses cannot be translated. `detail` is the original failure.
    #[error("Cannot map image {path}: {detail}")]
    ImageMapping
    {
        /// Loader-reported path of the image.
        path: std::path::PathBuf,
        /// Why its mapped header could not be read or parsed.
        detail: String,
    },

    /// Symbols for a loaded image could not be loaded for a reason other than missing
    /// debug information, for example because its file could not be read while Cargo
    /// replaced it. `source` is the original failure. The session keeps the failure
    /// only while the files the load read are unchanged: later requests return it again
    /// without reading them, and the first request after the image, its `.dSYM` or the
    /// debug cache changes on disk tries the load again.
    #[error("Cannot load symbols for {path}: {source}")]
    SymbolLoad
    {
        /// Loader-reported path of the image.
        path: std::path::PathBuf,
        /// The original failure.
        source: Box<DebuggerError>,
    },

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
    /// Launch or attach to a process before performing operations on it.
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
    /// Wait for the process to stop (for example with
    /// [`Session::wait`](crate::session::Session::wait)) before performing operations
    /// that require it to be stopped.
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
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Session;
    /// use ferros_core::breakpoints::BreakpointRequest;
    /// use ferros_core::error::DebuggerError;
    /// use ferros_core::types::Address;
    /// # fn example(session: &mut Session) -> ferros_core::Result<()> {
    ///
    /// # let address = Address::from(0x1000);
    /// let breakpoint_id = match session.add_breakpoint(BreakpointRequest::Hardware { address }) {
    ///     Err(DebuggerError::ResourceExhausted(msg)) => {
    ///         println!("Hardware breakpoint limit reached: {}", msg);
    ///         // Try software breakpoint instead
    ///         session.add_breakpoint(BreakpointRequest::Software { address })?
    ///     }
    ///     Ok(id) => id,
    ///     Err(e) => return Err(e),
    /// };
    /// # Ok(())
    /// # }
    /// ```
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
    ///
    /// ## Context
    ///
    /// - `operation`: Description of what operation was being performed (e.g., "read ARM64 registers")
    /// - `thread_id`: Optional thread ID if the operation was thread-specific
    /// - `details`: Additional error details from the underlying system call
    #[error("Failed to read registers: {operation}")]
    ReadRegistersFailed
    {
        /// Description of the operation that failed
        operation: String,
        /// Thread ID if the operation was thread-specific
        thread_id: Option<crate::types::ThreadId>,
        /// Additional error details
        details: String,
    },

    /// Any other operating-system failure, carrying the raw OS code and message.
    #[error("OS error {code}: {message}")]
    Os
    {
        /// Raw error code reported by the operating system (errno, kern_return_t, ...).
        code: i64,
        /// Human-readable description of the failure.
        message: String,
    },

    /// I/O error (for file operations, etc.)
    ///
    /// Used for errors when reading/writing files, sockets, etc.
    /// This is a standard Rust `std::io::Error` converted to our error type.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A process is already being debugged, so `launch` or `attach` is refused.
    ///
    /// Returned by [`Session::launch`](crate::session::Session::launch) and
    /// [`Session::attach`](crate::session::Session::attach) while the session is
    /// stopped or running. Detach first, or wait for the process to exit.
    #[error("A process is already being debugged")]
    AlreadyAttached,

    /// Operation requires the process to be running.
    ///
    /// Returned by [`Session::wait`](crate::session::Session::wait) with no timeout
    /// while the process is stopped and no stop event is pending, since that wait
    /// would never return.
    #[error("Process must be running for this operation")]
    NotRunning,

    /// The named thread does not belong to the debugged process.
    #[error("No thread with id {0}")]
    ThreadNotFound(u64),

    /// The operation needs a selected thread, but the process has no thread to select.
    #[error("No thread is selected")]
    NoThreadSelected,

    /// An error reported by a [`Backend`](crate::backend::Backend), kept as is.
    ///
    /// [`BackendError::PermissionDenied`] is not wrapped: it converts to
    /// [`DebuggerError::PermissionDenied`] instead, so callers that match on that
    /// variant keep working.
    #[error(transparent)]
    Backend(BackendError),

    /// A source location or function name could not be translated to addresses: an
    /// unknown or ambiguous file or function, a line with no code, or unreadable debug
    /// info. The [`SourceLookupError`] is kept as is, so callers can match on it.
    #[error(transparent)]
    SourceLookup(#[from] SourceLookupError),
}

impl DebuggerError
{
    /// A copy of this error, so a kept failure can be reported again. An [`Io`](Self::Io)
    /// error keeps its kind and OS code (or, without one, its message) but not a custom
    /// payload's type.
    pub(crate) fn duplicate(&self) -> Self
    {
        match self {
            Self::ProcessNotFound(pid) => Self::ProcessNotFound(*pid),
            Self::PermissionDenied(message) => Self::PermissionDenied(message.clone()),
            Self::InvalidArgument(message) => Self::InvalidArgument(message.clone()),
            Self::NoDebugInfo { path, detail } => Self::NoDebugInfo {
                path: path.clone(),
                detail: detail.clone(),
            },
            Self::ImageMapping { path, detail } => Self::ImageMapping {
                path: path.clone(),
                detail: detail.clone(),
            },
            Self::SymbolLoad { path, source } => Self::SymbolLoad {
                path: path.clone(),
                source: Box::new(source.duplicate()),
            },
            Self::AttachFailed(message) => Self::AttachFailed(message.clone()),
            Self::NotAttached => Self::NotAttached,
            Self::NotStopped => Self::NotStopped,
            Self::NoBreakpoint(address) => Self::NoBreakpoint(*address),
            Self::BreakpointIdNotFound(id) => Self::BreakpointIdNotFound(*id),
            Self::ResourceExhausted(message) => Self::ResourceExhausted(message.clone()),
            Self::SuspendFailed(message) => Self::SuspendFailed(message.clone()),
            Self::ResumeFailed(message) => Self::ResumeFailed(message.clone()),
            Self::ReadRegistersFailed {
                operation,
                thread_id,
                details,
            } => Self::ReadRegistersFailed {
                operation: operation.clone(),
                thread_id: *thread_id,
                details: details.clone(),
            },
            Self::Os { code, message } => Self::Os {
                code: *code,
                message: message.clone(),
            },
            Self::Io(error) => Self::Io(match error.raw_os_error() {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None => std::io::Error::new(error.kind(), error.to_string()),
            }),
            Self::AlreadyAttached => Self::AlreadyAttached,
            Self::NotRunning => Self::NotRunning,
            Self::ThreadNotFound(id) => Self::ThreadNotFound(*id),
            Self::NoThreadSelected => Self::NoThreadSelected,
            Self::Backend(error) => Self::Backend(error.clone()),
            Self::SourceLookup(error) => Self::SourceLookup(error.clone()),
        }
    }
}

impl From<BackendError> for DebuggerError
{
    fn from(error: BackendError) -> Self
    {
        match error {
            BackendError::PermissionDenied(message) => Self::PermissionDenied(message),
            other => Self::Backend(other),
        }
    }
}

/// Convenience type alias for `Result<T, DebuggerError>`
///
/// This makes error handling more ergonomic. Instead of writing:
/// ```rust
/// use ferros_core::error::DebuggerError;
/// fn foo() -> std::result::Result<(), DebuggerError>
/// {
///     Ok(())
/// }
/// ```
/// We can write:
/// ```rust
/// use ferros_core::error::Result;
/// fn foo() -> Result<()>
/// {
///     Ok(())
/// }
/// ```
pub type Result<T> = std::result::Result<T, DebuggerError>;
