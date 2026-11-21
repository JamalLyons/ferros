//! Integration test fixtures and harness helpers for Ferros.

pub mod arm64;
pub mod control;

pub mod fixture;
#[cfg(target_os = "macos")]
pub mod harness;
pub mod scratch;
#[cfg(target_os = "macos")]
mod staging;
#[cfg(target_os = "macos")]
pub mod symbols;
#[cfg(target_os = "macos")]
pub mod transcript;

pub use fixture::{Fixture, MarkerError, find_marker};
