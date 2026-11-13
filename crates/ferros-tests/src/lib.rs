//! Integration test fixtures and harness helpers for Ferros.

pub mod fixture;
#[cfg(target_os = "macos")]
pub mod harness;
#[cfg(target_os = "macos")]
pub mod symbols;

pub use fixture::{Fixture, MarkerError, find_marker};
