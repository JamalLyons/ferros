//! # Ferros Utilities
//!
//! Logging for the Ferros workspace, built on `tracing`.
//!
//! This crate initializes logging and re-exports the `tracing` macros. It does
//! not define its own macros, a config type, or other helpers.

pub mod logging;

// Re-export commonly used logging functions for convenience
pub use logging::{LogFormat, LogLevel, init_logging, init_logging_for_tui, init_logging_to_file, init_logging_with_level};
pub use tracing::{debug, error, info, trace, warn};
