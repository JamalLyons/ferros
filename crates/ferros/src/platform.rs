//! Selection of the debugger implementation for the host operating system.
//!
//! `ferros-core` holds no OS-specific code, so the choice of implementation is made
//! here, in the binary. This module is the only place outside the platform crates
//! (and the test crate) that switches on the target operating system.

use ferros_core::Debugger;

/// Create the debugger implementation for the current operating system.
///
/// - macOS: `ferros_macos::MacOSDebugger`
/// - any other OS: not implemented yet, returns an error
///
/// # Errors
///
/// On macOS, returns the error from `MacOSDebugger::new()`. On any other OS, returns
/// `DebuggerError::AttachFailed` naming the unsupported platform.
pub fn create_debugger() -> ferros_core::Result<Box<dyn Debugger>>
{
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(ferros_macos::MacOSDebugger::new()?))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(ferros_core::DebuggerError::AttachFailed(format!(
            "Debugger not yet implemented for platform: {}",
            std::env::consts::OS
        )))
    }
}
