//! Terminal job control while the program runs (issue #122).
//!
//! A launched program has its own background process group, so the terminal's
//! job-control keys reach only ferros. [`JobControl`] is what the command loop needs from
//! the platform around every run of the program:
//!
//! - Ctrl-Z (`SIGTSTP`) while the program runs interrupts it first; once the stop is
//!   printed, ferros stops itself. After `fg` nothing is resumed until the user continues.
//! - With `launch --terminal`, the program's group gets the terminal foreground (and
//!   its own terminal settings) while it runs, so it can read typed input; ferros takes
//!   both back at each stop.
//!
//! `platform::job_control` builds the implementation for the host OS.

use std::io;

use ferros_core::ProcessId;

/// Job control around each run of the program.
pub trait JobControl
{
    /// The program is about to run. `group` is its own process group, if it has one.
    ///
    /// # Errors
    ///
    /// The terminal could not be handed over.
    fn program_runs(&mut self, group: Option<ProcessId>) -> io::Result<()>;

    /// Whether Ctrl-Z was pressed since [`program_runs`](Self::program_runs).
    fn suspend_requested(&self) -> bool;

    /// The program stopped (or ended): take the terminal back. Returns whether Ctrl-Z was
    /// pressed while it ran, in which case the caller prints the stop and calls
    /// [`suspend`](Self::suspend).
    ///
    /// # Errors
    ///
    /// The terminal could not be taken back.
    fn program_stopped(&mut self) -> io::Result<bool>;

    /// Stop ferros as Ctrl-Z would; returns after `fg`.
    ///
    /// # Errors
    ///
    /// The OS refused the signal.
    fn suspend(&mut self) -> io::Result<()>;
}

/// No job control: Ctrl-Z keeps its default action and the terminal is never handed over.
#[derive(Debug, Default)]
pub struct NoJobControl;

impl JobControl for NoJobControl
{
    fn program_runs(&mut self, _group: Option<ProcessId>) -> io::Result<()>
    {
        Ok(())
    }

    fn suspend_requested(&self) -> bool
    {
        false
    }

    fn program_stopped(&mut self) -> io::Result<bool>
    {
        Ok(false)
    }

    fn suspend(&mut self) -> io::Result<()>
    {
        Ok(())
    }
}
