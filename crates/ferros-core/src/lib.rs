//! # ferros-core
//!
//! Low-level debugging primitives and process control for Ferros.
//!
//! This crate provides the foundational debugging capabilities, including:
//! - Process attachment and control
//! - Register inspection and manipulation
//! - Memory reading/writing (future)
//! - Breakpoint management (future)
//!
//! ## Platform Support
//!
//! - **macOS**: Uses Mach APIs (`task_for_pid`, `thread_get_state`, etc.)
//! - **Linux**: Will use `ptrace` (future)
//! - **Windows**: Will use Windows Debug API (future)
//!
//! ## Why unsafe code is needed
//!
//! This crate requires `unsafe` code because we're calling low-level system APIs
//! that interact directly with the kernel. These APIs are inherently unsafe
//! because they can:
//! - Access memory of other processes
//! - Modify process state
//! - Bypass normal Rust safety guarantees
//!
//! We wrap these unsafe calls in safe abstractions, but the underlying system
//! calls themselves must be `unsafe`.

#![allow(unsafe_code)] // Required for low-level system APIs (Mach, ptrace, etc.)

pub mod debugger;
pub mod error;
pub mod platform;
pub mod types;

pub use debugger::Debugger;
// Re-export commonly used types
pub use error::{DebuggerError, Result};
#[cfg(target_os = "macos")]
pub use platform::macos::MacOSDebugger;
pub use types::{ProcessId, Registers};
