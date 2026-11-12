//! # ferros-core
//!
//! Low-level debugging primitives and process control for Ferros.
//!
//! This crate provides the foundational debugging capabilities, including:
//! - Process attachment and control
//! - Register inspection and manipulation
//! - Memory reading and writing
//! - Software breakpoint management
//!
//! ## Platform Support
//!
//! ferros-core holds no OS-specific code and no `unsafe`: it does not depend on any
//! operating-system API. It defines the portable interface ([`backend::Backend`]), the
//! portable engine ([`session::Session`]) and the OS-independent pieces (types, symbols,
//! breakpoint bookkeeping, CPU knowledge). Each operating system is implemented in its
//! own crate:
//!
//! - **macOS**: the `ferros-macos` crate (`MacOSBackend`, using Mach APIs)
//! - **Linux**: Will use `ptrace` (future)
//! - **Windows**: Will use Windows Debug API (future)
//!
//! ## Backend boundary
//!
//! [`backend::Backend`] is the portable interface every operating system implements:
//! process control, register and memory access, and stop reporting (mechanism only).
//! [`arch::Arch`] holds CPU knowledge (trap bytes, pc/sp/fp/link register, pointer
//! authentication) independent of the OS. Neither module contains OS-specific code.
//!
//! ## Session engine
//!
//! [`session::Session`] is the portable engine: it owns a `Box<dyn Backend>` and holds
//! the policy (state machine, software breakpoint table, stack traces and
//! symbolication) that works the same with every backend.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod arch;
pub mod backend;
pub mod breakpoints;
pub mod error;
pub mod events;
pub mod session;
pub mod symbols;
pub mod types;

pub use arch::Arch;
pub use backend::{Backend, BackendError, BackendResult, LaunchSpec, ResumeRequest, StopEvent, ThreadAction};
pub use breakpoints::{BreakpointId, BreakpointInfo, BreakpointKind, BreakpointRequest, BreakpointState, WatchpointAccess};
// Re-export commonly used types
pub use error::{DebuggerError, Result};
pub use events::{DebuggerEvent, DebuggerEventReceiver, DebuggerEventSender, format_stop_reason};
pub use session::{Session, SessionState};
pub use symbols::{SymbolCache, SymbolFrame, Symbolication, TypeField, TypeKind, TypeSummary, TypeVariant};
pub use types::{
    Address, Architecture, FloatingPointState, FrameId, FrameKind, FrameStatus, MemoryRegion, MemoryRegionId, ProcessId,
    RegisterId, Registers, SourceLocation, StackFrame, StopReason, SymbolLanguage, SymbolName, ThreadId,
    VectorRegisterValue,
};
