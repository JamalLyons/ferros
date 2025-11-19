//! Stack frame types.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::symbols::{SourceLocation, SymbolName};
use super::{Address, ThreadId};

/// Stable identifier for a logical stack frame.
///
/// A `FrameId` uniquely identifies a stack frame within a thread's call stack.
/// It's computed from the thread ID, stack depth, inline depth, program counter,
/// and stack pointer to ensure uniqueness and stability across debugger operations.
///
/// ## Stability
///
/// Frame IDs are stable across multiple unwinding operations as long as the frame's
/// key characteristics (thread, depth, PC, SP) remain unchanged. This allows
/// frontends to track frame selections and maintain UI state.
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::{Address, FrameId, ThreadId};
///
/// let thread = ThreadId::from(12345);
/// let pc = Address::from(0x1000);
/// let sp = Address::from(0x7fff00000000);
/// let frame_id = FrameId::new(thread, 0, 0, pc, sp);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameId(u128);

impl FrameId
{
    /// Build an identifier from thread, depth, inline depth, and key addresses.
    ///
    /// Creates a stable frame identifier by hashing the thread ID, stack depth,
    /// inline depth, program counter, and stack pointer. The resulting 128-bit
    /// value uniquely identifies the frame.
    ///
    /// ## Parameters
    ///
    /// - `thread`: The thread that owns this frame
    /// - `depth`: The stack depth (0 = topmost frame, 1 = caller, etc.)
    /// - `inline_depth`: The inline depth within the physical frame (0 = innermost)
    /// - `pc`: The program counter address for this frame
    /// - `sp`: The stack pointer address for this frame
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Address, FrameId, ThreadId};
    ///
    /// let thread = ThreadId::from(12345);
    /// let frame_id = FrameId::new(
    ///     thread,
    ///     0,
    ///     0,
    ///     Address::from(0x1000),
    ///     Address::from(0x7fff00000000),
    /// );
    /// ```
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

    /// Get the raw `u128` representation suitable for serialization.
    ///
    /// Returns the underlying 128-bit value that can be serialized to JSON,
    /// binary formats, or stored in databases.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Address, FrameId, ThreadId};
    ///
    /// let frame_id = FrameId::new(
    ///     ThreadId::from(12345),
    ///     0,
    ///     0,
    ///     Address::from(0x1000),
    ///     Address::from(0x7fff00000000),
    /// );
    /// let raw_value = frame_id.as_u128();
    /// // Can be serialized: serde_json::to_string(&raw_value)
    /// ```
    pub fn as_u128(self) -> u128
    {
        self.0
    }
}

/// Differentiates physical vs. synthesized inline frames.
///
/// Stack frames can be either "physical" (actual function calls that consume
/// stack memory) or "inlined" (synthesized from DWARF debug information to show
/// inline function calls that were optimized away by the compiler).
///
/// ## Physical Frames
///
/// Physical frames represent actual function calls that have their own stack
/// frame with local variables, saved registers, and return addresses.
///
/// ## Inlined Frames
///
/// Inlined frames are synthesized from DWARF debug information to show inline
/// function calls that were optimized away. They don't have their own stack
/// frame but share storage with their parent physical frame.
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::{Address, FrameId, FrameKind, ThreadId};
///
/// // A physical frame
/// let physical_id = FrameId::new(
///     ThreadId::from(12345),
///     0,
///     0,
///     Address::from(0x1000),
///     Address::from(0x7fff00000000),
/// );
/// let physical = FrameKind::Physical;
///
/// // An inlined frame within the physical frame
/// let inlined = FrameKind::Inlined {
///     physical: physical_id,
///     depth: 0, // Innermost inline call
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind
{
    /// Actual stack frame that consumed stack memory.
    ///
    /// This represents a real function call with its own stack frame, local
    /// variables, saved registers, and return address.
    Physical,
    /// DWARF inline instance that shares storage with the `physical` frame id.
    ///
    /// This represents an inline function call that was optimized away by the
    /// compiler but is shown in the stack trace for debugging purposes. The
    /// inline frame shares the same stack storage as its parent physical frame.
    Inlined
    {
        /// Parent physical frame that contains this inline frame.
        physical: FrameId,
        /// Depth inside the inline chain (0 = innermost inline call).
        ///
        /// When multiple functions are inlined, this indicates the nesting level.
        /// A depth of 0 means this is the innermost inline call.
        depth: u8,
    },
}

impl FrameKind
{
    /// Returns `true` if this represents an inline-only frame.
    ///
    /// Inline frames are synthesized from DWARF debug information and don't
    /// have their own stack storage. Physical frames return `false`.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Address, FrameId, FrameKind, ThreadId};
    ///
    /// let physical = FrameKind::Physical;
    /// assert!(!physical.is_inlined());
    ///
    /// let inlined = FrameKind::Inlined {
    ///     physical: FrameId::new(
    ///         ThreadId::from(12345),
    ///         0,
    ///         0,
    ///         Address::from(0x1000),
    ///         Address::from(0x7fff00000000),
    ///     ),
    ///     depth: 0,
    /// };
    /// assert!(inlined.is_inlined());
    /// ```
    pub const fn is_inlined(self) -> bool
    {
        matches!(self, FrameKind::Inlined { .. })
    }
}

/// Indicates how reliable a frame's unwind data is.
///
/// The quality of stack unwinding can vary depending on the availability of
/// debug information and the unwinding method used. This enum indicates the
/// reliability of the frame's data.
///
/// ## Reliability Levels
///
/// - `Complete`: Most reliable - full debug information available
/// - `CfiFallback`: Moderate reliability - using frame pointers or link register
/// - `Heuristic`: Least reliable - best-effort reconstruction
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::FrameStatus;
///
/// // Check frame reliability
/// let frame_status = FrameStatus::Complete;
/// match frame_status {
///     FrameStatus::Complete => println!("Frame data is fully reliable"),
///     FrameStatus::CfiFallback => println!("Frame data may have minor inaccuracies"),
///     FrameStatus::Heuristic => println!("Frame data may be inaccurate"),
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameStatus
{
    /// CFI (Call Frame Information) provided full register restoration.
    ///
    /// This is the most reliable status. The frame was unwound using complete
    /// DWARF CFI (Call Frame Information) data, which provides accurate register
    /// restoration and frame boundaries.
    Complete,
    /// DWARF CFI missing; fallback to frame pointers or link register.
    ///
    /// This indicates moderate reliability. The frame was unwound using frame
    /// pointers (RBP/FP) or the link register (LR), which is less accurate than
    /// CFI but still generally reliable for most cases.
    CfiFallback,
    /// Heuristic or best-effort reconstruction (may be inaccurate).
    ///
    /// This is the least reliable status. The frame was reconstructed using
    /// heuristics or best-effort methods when debug information is unavailable.
    /// The frame data may be inaccurate or incomplete.
    Heuristic,
}

/// Logical stack frame (physical or inline).
///
/// A `StackFrame` represents a single frame in a thread's call stack. It can be
/// either a physical frame (actual function call) or an inline frame (synthesized
/// from DWARF debug information).
///
/// ## Frame Information
///
/// Each frame contains:
/// - **Addresses**: Program counter (PC), stack pointer (SP), frame pointer (FP)
/// - **Symbol Information**: Function name and source location (if available)
/// - **Metadata**: Frame ID, thread, index, kind, and reliability status
///
/// ## Physical vs. Inline Frames
///
/// - **Physical frames**: Real function calls with their own stack storage
/// - **Inline frames**: Synthesized from DWARF to show optimized-away inline calls
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::{Address, FrameId, FrameKind, FrameStatus, StackFrame, ThreadId};
///
/// let frame = StackFrame {
///     id: FrameId::new(
///         ThreadId::from(12345),
///         0,
///         0,
///         Address::from(0x1000),
///         Address::from(0x7fff00000000),
///     ),
///     thread: ThreadId::from(12345),
///     index: 0,
///     kind: FrameKind::Physical,
///     pc: Address::from(0x1000),
///     sp: Address::from(0x7fff00000000),
///     fp: Address::from(0x7fff00001000),
///     return_address: Some(Address::from(0x2000)),
///     symbol: None,
///     location: None,
///     status: FrameStatus::Complete,
/// };
/// ```
#[derive(Debug, Clone)]
pub struct StackFrame
{
    /// Stable identifier for frontends to diff selections.
    ///
    /// This ID is stable across multiple unwinding operations and can be used
    /// by frontends to track frame selections and maintain UI state.
    pub id: FrameId,
    /// Owning thread that this frame belongs to.
    pub thread: ThreadId,
    /// Ordered index within the stack trace (includes inline frames).
    ///
    /// The topmost frame (most recent call) has index 0, the caller has index 1,
    /// and so on. Inline frames are included in this ordering.
    pub index: usize,
    /// Whether this is a physical or inline frame.
    pub kind: FrameKind,
    /// Program counter corresponding to this frame.
    ///
    /// This is the address of the instruction that was executing (or about to
    /// execute) when this frame was active.
    pub pc: Address,
    /// Stack pointer snapshot at this frame.
    ///
    /// This is the value of the stack pointer (SP) when this frame was active.
    pub sp: Address,
    /// Frame/base pointer snapshot if available.
    ///
    /// This is the value of the frame pointer (FP/RBP) when this frame was active.
    /// May be zero if frame pointers are not available or not used.
    pub fp: Address,
    /// Return address that unwinding will jump to (if known).
    ///
    /// This is the address where execution will resume after this function returns.
    /// Used for stack unwinding to the caller frame.
    pub return_address: Option<Address>,
    /// Best-effort symbol for the frame.
    ///
    /// Contains the function name (demangled if available) if symbol information
    /// is available. May be `None` if symbols are not available or the address
    /// doesn't correspond to a known symbol.
    pub symbol: Option<SymbolName>,
    /// Best-effort source location.
    ///
    /// Contains the source file, line number, and column if debug information
    /// is available. May be `None` if source information is not available.
    pub location: Option<SourceLocation>,
    /// Reliability indicator for this frame's unwind data.
    ///
    /// Indicates how reliable the frame's data is based on the unwinding method
    /// used (CFI, frame pointers, or heuristics).
    pub status: FrameStatus,
}

impl StackFrame
{
    /// Convenience helper to test if this is an inline frame.
    ///
    /// Returns `true` if this frame is an inline frame (synthesized from DWARF),
    /// or `false` if it's a physical frame (actual function call).
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Address, FrameId, FrameKind, StackFrame, ThreadId};
    ///
    /// let frame = StackFrame {
    ///     id: FrameId::new(
    ///         ThreadId::from(12345),
    ///         0,
    ///         0,
    ///         Address::from(0x1000),
    ///         Address::from(0x7fff00000000),
    ///     ),
    ///     thread: ThreadId::from(12345),
    ///     index: 0,
    ///     kind: FrameKind::Physical,
    ///     pc: Address::from(0x1000),
    ///     sp: Address::from(0x7fff00000000),
    ///     fp: Address::ZERO,
    ///     return_address: None,
    ///     symbol: None,
    ///     location: None,
    ///     status: ferros_core::types::FrameStatus::Complete,
    /// };
    ///
    /// assert!(!frame.is_inlined());
    /// ```
    pub fn is_inlined(&self) -> bool
    {
        self.kind.is_inlined()
    }
}
