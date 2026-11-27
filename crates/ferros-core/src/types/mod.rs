//! # Types
//!
//! Platform-agnostic types used throughout the debugger.
//!
//! These types abstract away platform-specific details, allowing the rest of
//! the debugger to work with concepts like "process ID" and "registers" without
//! knowing whether we're on macOS, Linux, or Windows.

pub mod address;
pub mod build_id;
pub mod process;
pub mod registers;
pub mod stack;
pub mod symbols;

// Re-export all public types
pub use address::Address;
pub use build_id::{BuildId, BuildIdentity, same_build};
pub use process::{Architecture, MemoryRegion, MemoryRegionId, ProcessId, StopReason, ThreadId};
pub use registers::{Arm64Register, FloatingPointState, RegisterId, Registers, VectorRegisterValue, X86_64Register};
pub use stack::{FrameId, FrameKind, FrameRegisters, FrameStatus, StackFrame};
pub use symbols::{FunctionParameter, SourceLocation, SymbolLanguage, SymbolName, SymbolSource};
