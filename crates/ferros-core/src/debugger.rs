//! # Debugger Module
//!
//! This module contains the `FerrosDebugger` trait and its implementations.
//!
//! The `FerrosDebugger` trait defines the operations a debugger can perform on a process.
//! All platform-specific implementations (macOS, Linux, Windows) must
//! implement these methods.

use crate::error::FerrosResult;

/// Main debugger interface
///
/// This trait defines the operations a debugger can perform on a process.
/// All platform-specific implementations (macOS, Linux, Windows) must
/// implement these methods.
///
/// ## Lifecycle
///
/// 1. Create a debugger: `MacOSDebugger::new()`
/// 2. Attach to a process: `attach(pid)`
/// 3. Inspect/manipulate: `read_registers()`, `write_registers()`, etc.
/// 4. Detach: `detach()`
pub trait FerrosDebugger
{
    /// Launch a new process under debugger control
    ///
    /// Spawns a new process from the given executable path and arguments, and
    /// immediately attaches the debugger to it. The process starts in a suspended
    /// state, allowing you to set breakpoints before it begins execution.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Uses `posix_spawn()` with `POSIX_SPAWN_START_SUSPENDED` flag,
    ///   then calls `attach()` to get the task port. This avoids permission issues
    ///   that can occur when attaching to already-running processes.
    /// - **Linux**: Will use `fork()` + `execve()` with `PTRACE_TRACEME`
    /// - **Windows**: Will use `CreateProcess()` with `DEBUG_PROCESS` flag
    ///
    /// ## Parameters
    ///
    /// - `program`: Path to the executable to launch
    /// - `args`: Command-line arguments (first argument should be the program name)
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Invalid program path or arguments
    /// - `AttachFailed`: Failed to spawn process or attach to it
    /// - `Io`: I/O error (e.g., file not found, permission denied)
    fn launch(&mut self, program: &str, args: &[&str]) -> FerrosResult<crate::types::process::ProcessId>;

    /// Attach to a running process
    ///
    /// This establishes a connection to the target process, allowing you
    /// to inspect and control it. The process continues running normally
    /// after attachment.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Calls `task_for_pid()` to get a Mach task port
    /// - **Linux**: Will call `ptrace(PTRACE_ATTACH, pid)`
    /// - **Windows**: Will call `DebugActiveProcess(pid)`
    ///
    /// ## Errors
    ///
    /// - `ProcessNotFound`: The PID doesn't exist
    /// - `PermissionDenied`: Insufficient permissions (need sudo/entitlements)
    /// - `AttachFailed`: Other attachment failures (e.g., failed to enumerate threads)
    fn attach(&mut self, pid: crate::types::process::ProcessId) -> FerrosResult<()>;

    /// Detach from the process
    ///
    /// Releases the connection to the target process. After detaching,
    /// you can no longer inspect or control the process.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Releases the Mach task port (no explicit detach needed)
    /// - **Linux**: Will call `ptrace(PTRACE_DETACH, pid)`
    /// - **Windows**: Will call `DebugActiveProcessStop(pid)`
    ///
    /// ## Note
    ///
    /// On macOS, detaching doesn't actually do anything - the task port
    /// is automatically released when the debugger struct is dropped.
    /// But we provide this method for consistency across platforms.
    fn detach(&mut self) -> FerrosResult<()>;
}

/// Factory function to create a platform-specific debugger
///
/// This function automatically creates the correct debugger implementation
/// for the current platform. It uses conditional compilation (`#[cfg]`)
/// to select the right implementation at compile time.
///
/// ## Why a factory function?
///
/// - **Convenience**: Users don't need to know which debugger type to use
/// - **Platform abstraction**: Same code works on all platforms
/// - **Type erasure**: Returns `Box<dyn Debugger>` so you can store it generically
///
/// ## Platform Support
///
/// - macOS: Returns `MacOSDebugger`
/// - Linux: Will return `LinuxDebugger` (future)
/// - Windows: Will return `WindowsDebugger` (future)
pub fn create_debugger() -> FerrosResult<Box<dyn FerrosDebugger>>
{
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::platform::macos::MacOSDebugger::new()?))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(crate::error::FerrosError::PlatformNotSupported {
            platform: std::env::consts::OS.to_string(),
            message: format!("Debugger not yet implemented for platform: {}", std::env::consts::OS),
        })
    }
}
