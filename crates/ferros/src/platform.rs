//! Selection of the backend, the object format and the job control for the host operating system.
//!
//! `ferros-core` holds no OS-specific code, so the choice of backend is made here, in
//! the binary. This module is the only place outside the platform crates (and the test
//! crate) that switches on the target operating system.

use std::io;

use ferros_core::Session;

use crate::repl::jobs::JobControl;

/// Create a [`Session`] for the current operating system: its backend and the object
/// format of its images.
///
/// - macOS: `ferros_macos::MacOSBackend` with `ferros_macos::MachOFormat`
/// - any other OS: not implemented yet, returns an error
///
/// # Errors
///
/// On macOS, returns the error from `MacOSBackend::new()`. On any other OS, returns
/// `DebuggerError::Backend(BackendError::Unsupported(..))` naming the platform.
pub fn create_session() -> ferros_core::Result<Session>
{
    #[cfg(target_os = "macos")]
    {
        ferros_macos::new_session()
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(ferros_core::DebuggerError::Backend(ferros_core::BackendError::Unsupported(
            format!("no debugger backend for platform: {}", std::env::consts::OS),
        )))
    }
}

/// Create the [`JobControl`] for the current operating system. With `give_terminal`, the
/// program's group gets the terminal foreground while it runs (`launch --terminal`).
/// Without a terminal on stdin there is no Ctrl-Z to catch, so nothing is done.
///
/// # Errors
///
/// With `give_terminal`: stdin is not a terminal, ferros is not its foreground job, or
/// the OS has no job control support in ferros yet.
pub fn job_control(give_terminal: bool, stdin_is_terminal: bool) -> io::Result<Box<dyn JobControl>>
{
    if !give_terminal && !stdin_is_terminal {
        return Ok(Box::new(crate::repl::jobs::NoJobControl));
    }

    #[cfg(target_os = "macos")]
    {
        let foreground = if give_terminal {
            Some(ferros_macos::terminal::Foreground::new()?)
        } else {
            None
        };
        Ok(Box::new(macos::Jobs {
            catcher: None,
            foreground,
            handed_over: false,
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        if give_terminal {
            return Err(io::Error::other(format!(
                "--terminal is not supported on {}",
                std::env::consts::OS
            )));
        }
        Ok(Box::new(crate::repl::jobs::NoJobControl))
    }
}

#[cfg(target_os = "macos")]
mod macos
{
    use std::io;

    use ferros_core::ProcessId;
    use ferros_macos::terminal::{Foreground, SuspendCatcher, suspend_process_group};

    use crate::repl::jobs::JobControl;

    /// `SIGTSTP` is caught while the program runs; the terminal is optionally handed over.
    pub struct Jobs
    {
        pub catcher: Option<SuspendCatcher>,
        pub foreground: Option<Foreground>,
        pub handed_over: bool,
    }

    impl JobControl for Jobs
    {
        fn program_runs(&mut self, group: Option<ProcessId>) -> io::Result<()>
        {
            self.catcher = Some(SuspendCatcher::install()?);
            if let (Some(foreground), Some(group)) = (&mut self.foreground, group) {
                foreground.give(group)?;
                self.handed_over = true;
            }
            Ok(())
        }

        fn suspend_requested(&self) -> bool
        {
            self.catcher.as_ref().is_some_and(SuspendCatcher::requested)
        }

        fn program_stopped(&mut self) -> io::Result<bool>
        {
            let requested = self.suspend_requested();
            // Restores the previous (default) SIGTSTP action.
            self.catcher = None;
            if std::mem::take(&mut self.handed_over)
                && let Some(foreground) = &mut self.foreground
            {
                foreground.take_back()?;
            }
            Ok(requested)
        }

        fn suspend(&mut self) -> io::Result<()>
        {
            suspend_process_group()
        }
    }
}
