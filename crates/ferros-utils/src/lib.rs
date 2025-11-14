//! # Ferros Utilities
//!
//! Shared utilities, logging, config, and helpers for Ferros.
//!
//! This crate provides common functionality used across the Ferros workspace,
//! including production-ready logging infrastructure built on `tracing`.

pub mod logging;

// Re-export commonly used logging functions for convenience
pub use logging::{init_logging, init_logging_with_level, LogFormat, LogLevel};
