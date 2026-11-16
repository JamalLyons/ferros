//! Selection of the backend for the host operating system.
//!
//! `ferros-core` holds no OS-specific code, so the choice of backend is made here, in
//! the binary. This module is the only place outside the platform crates (and the test
//! crate) that switches on the target operating system.

use ferros_core::Backend;

/// Create the [`Backend`] for the current operating system.
///
/// - macOS: `ferros_macos::MacOSBackend`
/// - any other OS: not implemented yet, returns an error
///
/// # Errors
///
/// On macOS, returns the error from `MacOSBackend::new()`. On any other OS, returns
/// `DebuggerError::Backend(BackendError::Unsupported(..))` naming the platform.
pub fn create_backend() -> ferros_core::Result<Box<dyn Backend>>
{
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(ferros_macos::MacOSBackend::new()?))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(ferros_core::DebuggerError::Backend(ferros_core::BackendError::Unsupported(
            format!("no debugger backend for platform: {}", std::env::consts::OS),
        )))
    }
}

/// Run one resume/wait cycle with terminal ownership lent to a launched group.
/// Attached targets keep their terminal policy.
pub fn with_terminal_foreground(
    pid: Option<ferros_core::ProcessId>,
    run: impl FnOnce() -> ferros_core::Result<ferros_core::StopEvent>,
) -> ferros_core::Result<ferros_core::StopEvent>
{
    #[cfg(target_os = "macos")]
    {
        let mut terminal = pid.map(ferros_macos::launch::ForegroundTerminal::give_to).transpose()?;
        let result = run();
        if let Some(terminal) = &mut terminal {
            terminal.restore()?;
        }
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        run()
    }
}
