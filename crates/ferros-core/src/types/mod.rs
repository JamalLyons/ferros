//! # Types
//!
//! Platform-agnostic types used throughout the debugger.
//!
//! These types abstract away platform-specific details, allowing the rest of
//! the debugger to work with concepts like "process ID" and "registers" without
//! knowing whether we're on macOS, Linux, or Windows.

pub mod address;
pub mod process;

// Re-export all public types
pub use address::Address;
pub use process::ThreadId;
