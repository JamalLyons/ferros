//! Stack frame types.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::symbols::{SourceLocation, SymbolName};
use super::{Address, ThreadId};

/// Stable identifier for a logical stack frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameId(u128);

impl FrameId
{
    /// Build an identifier from thread, depth, inline depth, and key addresses.
    pub fn new(thread: ThreadId, depth: u32, inline_depth: u8, pc: Address, sp: Address) -> Self
    {
        let mut hasher_a = DefaultHasher::new();
        thread.hash(&mut hasher_a);
        depth.hash(&mut hasher_a);
        let upper = hasher_a.finish() as u128;

        let mut hasher_b = DefaultHasher::new();
        inline_depth.hash(&mut hasher_b);
        pc.hash(&mut hasher_b);
        sp.hash(&mut hasher_b);
        let lower = hasher_b.finish() as u128;

        Self((upper << 64) | lower)
    }

    /// Raw `u128` representation suitable for serialization.
    pub fn as_u128(self) -> u128
    {
        self.0
    }
}

/// Differentiates physical vs. synthesized inline frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind
{
    /// Actual stack frame that consumed stack memory.
    Physical,
    /// DWARF inline instance that shares storage with the `physical` frame id.
    Inlined
    {
        /// Parent physical frame.
        physical: FrameId,
        /// Depth inside the inline chain (0 = innermost).
        depth: u8,
    },
}

impl FrameKind
{
    /// Returns `true` if this represents an inline-only frame.
    pub const fn is_inlined(self) -> bool
    {
        matches!(self, FrameKind::Inlined { .. })
    }
}

/// Indicates how reliable a frame's unwind data is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameStatus
{
    /// CFI provided full register restoration.
    Complete,
    /// DWARF missing; fallback to frame pointers or link register.
    CfiFallback,
    /// Heuristic or best-effort reconstruction (may be inaccurate).
    Heuristic,
}

/// Logical stack frame (physical or inline).
#[derive(Debug, Clone)]
pub struct StackFrame
{
    /// Stable identifier for frontends to diff selections.
    pub id: FrameId,
    /// Owning thread.
    pub thread: ThreadId,
    /// Ordered index within the stack trace (includes inline frames).
    pub index: usize,
    /// Whether this is an inline frame.
    pub kind: FrameKind,
    /// Program counter corresponding to this frame.
    pub pc: Address,
    /// Stack pointer snapshot.
    pub sp: Address,
    /// Frame/base pointer snapshot if available.
    pub fp: Address,
    /// Return address that unwinding will jump to (if known).
    pub return_address: Option<Address>,
    /// Best-effort symbol for the frame.
    pub symbol: Option<SymbolName>,
    /// Best-effort source location.
    pub location: Option<SourceLocation>,
    /// Reliability indicator.
    pub status: FrameStatus,
}

impl StackFrame
{
    /// Convenience helper to test for inline frames.
    pub fn is_inlined(&self) -> bool
    {
        self.kind.is_inlined()
    }
}
