//! # ferros-ui
//!
//! Parked terminal UI for the Ferros debugger, built on `ratatui`.
//!
//! The crate is excluded from the workspace. It still names the removed
//! `ferros_core::Debugger` trait, so it does not build against `Session`.
//! `ferros launch` and `ferros attach` open the CLI REPL instead.

pub mod app;
pub mod event;
pub mod tui;
pub mod ui;
pub mod widgets;

pub use app::App;
pub use tui::Tui;

/// Create a [`Tui`] and run it with `debugger`.
///
/// `debugger` is the removed `Debugger` trait. This crate is parked and excluded
/// from the workspace, so this function is not built by `cargo test`.
///
/// # Errors
///
/// Returns an error if the terminal cannot be initialized or restored.
pub async fn run_tui(debugger: Box<dyn ferros_core::Debugger>, pid: Option<u32>, was_launched: bool) -> std::io::Result<()>
{
    let mut tui = Tui::new()?;
    tui.run(debugger, pid, was_launched).await
}
