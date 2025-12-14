//! Prelude for ferros-core
//!
//! This module exports the most commonly used types and functions from the ferros-core crate.
//! It is used to reduce the amount of boilerplate code needed to use the ferros-core crate.

pub use crate::error::{FerrosError, FerrosResult};
pub use crate::platform::macos::*;
pub use crate::types::address::Address;
pub use crate::types::process::{Architecture, MemoryRegion, MemoryRegionId, ProcessId, StopReason, ThreadId};
