//! # ferros-ui
//!
//! Terminal User Interface (TUI) for the Ferros debugger.
//!
//! This crate provides an interactive terminal interface for debugging Rust programs,
//! built on top of `ratatui`. It displays registers, memory, threads, and other
//! debugger state in a user-friendly format.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use ferros_core::debugger::create_debugger;
//! use ferros_core::types::ProcessId;
//! use ferros_ui::Tui;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut debugger = create_debugger()?;
//! debugger.attach(ProcessId::from(12345))?;
//!
//! let mut tui = Tui::new(debugger)?;
//! tui.run().await?;
//! # Ok(())
//! # }
//! ```

pub mod app;
pub mod event;
pub mod tui;
pub mod ui;
pub mod widgets;

pub use app::App;
pub use tui::Tui;

/// Run the TUI with a debugger instance
///
/// This is a convenience function that creates a TUI and runs it with the given debugger.
///
/// # Example
///
/// ```rust,no_run
/// use ferros_core::debugger::create_debugger;
/// use ferros_core::types::ProcessId;
/// use ferros_ui::run_tui;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut debugger = create_debugger()?;
/// debugger.attach(ProcessId::from(12345))?;
///
/// run_tui(debugger).await?;
/// # Ok(())
/// # }
/// ```
pub async fn run_tui(debugger: Box<dyn ferros_core::Debugger>, pid: Option<u32>, was_launched: bool) -> std::io::Result<()>
{
    let mut tui = Tui::new()?;
    tui.run(debugger, pid, was_launched).await
}
