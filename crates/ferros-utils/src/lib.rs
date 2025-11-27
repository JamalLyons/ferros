//! # Ferros Utilities
//!
//! Shared utilities, logging, config, and helpers for Ferros.
//!
//! This crate provides common functionality used across the Ferros workspace,
//! including a logging infrastructure built on `tracing`.

pub mod logging;

// Re-export commonly used logging functions for convenience
pub use logging::{LogFormat, LogLevel, init_logging, init_logging_for_tui, init_logging_with_level};
pub use tracing::{debug, error, info, trace, warn};
