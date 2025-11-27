//! # ferros-core
//!
//! Low-level debugging primitives and process control for Ferros.
//!
//! This crate provides the foundational debugging capabilities, including:
//! - Process attachment and control
//! - Register inspection and manipulation
//! - Memory reading/writing
//! - Breakpoint management
//!
//! ## Platform Support
//!
//! - **macOS**: Uses Mach APIs (`task_for_pid`, `thread_get_state`, etc.)
//! - **Linux**: TBA
//! - **Windows** TBA
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

#![allow(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod platform;
mod types;
