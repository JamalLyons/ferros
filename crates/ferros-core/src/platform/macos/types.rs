//! # macOS Platform-Specific Types
//!
//! Newtype wrappers for platform-specific values to improve type safety.
//!
//! These types prevent accidentally passing wrong values (e.g., passing a debug
//! state flavor where a thread state flavor is expected) and make APIs more
//! self-documenting.

use libc::c_int;

/// Thread state flavor identifier
///
/// This newtype wraps a `c_int` value that identifies which type of thread
/// state to read/write (e.g., `ARM_THREAD_STATE64`, `X86_THREAD_STATE64`).
///
/// ## Usage
///
/// ```rust,no_run
/// use ferros_core::platform::macos::constants;
/// use ferros_core::platform::macos::types::ThreadStateFlavor;
///
/// let flavor = ThreadStateFlavor::from(constants::ARM_THREAD_STATE64);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadStateFlavor(pub c_int);

impl ThreadStateFlavor
{
    /// Create a new thread state flavor from a raw `c_int` value.
    pub const fn new(value: c_int) -> Self
    {
        Self(value)
    }

    /// Get the raw `c_int` value.
    pub const fn value(self) -> c_int
    {
        self.0
    }
}

impl From<c_int> for ThreadStateFlavor
{
    fn from(value: c_int) -> Self
    {
        Self(value)
    }
}

impl From<ThreadStateFlavor> for c_int
{
    fn from(flavor: ThreadStateFlavor) -> Self
    {
        flavor.0
    }
}

/// Debug state flavor identifier
///
/// This newtype wraps a `c_int` value that identifies which type of debug
/// state to read/write (e.g., `ARM_DEBUG_STATE64`, `X86_DEBUG_STATE64`).
///
/// ## Usage
///
/// ```rust,no_run
/// use ferros_core::platform::macos::constants;
/// use ferros_core::platform::macos::types::DebugStateFlavor;
///
/// let flavor = DebugStateFlavor::from(constants::ARM_DEBUG_STATE64);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DebugStateFlavor(pub c_int);

impl DebugStateFlavor
{
    /// Create a new debug state flavor from a raw `c_int` value.
    pub const fn new(value: c_int) -> Self
    {
        Self(value)
    }

    /// Get the raw `c_int` value.
    pub const fn value(self) -> c_int
    {
        self.0
    }
}

impl From<c_int> for DebugStateFlavor
{
    fn from(value: c_int) -> Self
    {
        Self(value)
    }
}

impl From<DebugStateFlavor> for c_int
{
    fn from(flavor: DebugStateFlavor) -> Self
    {
        flavor.0
    }
}

/// Register array index
///
/// This newtype wraps a `usize` value that represents an index into a
/// register state array. It prevents accidentally mixing register indices
/// with other array indices or offsets.
///
/// ## Usage
///
/// ```rust,no_run
/// use ferros_core::platform::macos::constants;
/// use ferros_core::platform::macos::types::RegisterIndex;
///
/// let pc_index = RegisterIndex::from(constants::ARM64_PC_INDEX);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegisterIndex(pub usize);

impl RegisterIndex
{
    /// Create a new register index from a raw `usize` value.
    pub const fn new(value: usize) -> Self
    {
        Self(value)
    }

    /// Get the raw `usize` value.
    pub const fn value(self) -> usize
    {
        self.0
    }
}

impl From<usize> for RegisterIndex
{
    fn from(value: usize) -> Self
    {
        Self(value)
    }
}

impl From<RegisterIndex> for usize
{
    fn from(index: RegisterIndex) -> Self
    {
        index.0
    }
}

/// Hardware breakpoint slot number
///
/// This newtype wraps a `u32` value that represents a hardware breakpoint
/// slot (0-3 for x86-64, 0-15 for ARM64). It prevents accidentally mixing
/// breakpoint slots with other numeric values.
///
/// ## Usage
///
/// ```rust,no_run
/// use ferros_core::platform::macos::types::BreakpointSlot;
///
/// let slot = BreakpointSlot::new(0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BreakpointSlot(pub u32);

impl BreakpointSlot
{
    /// Create a new breakpoint slot from a raw `u32` value.
    pub const fn new(value: u32) -> Self
    {
        Self(value)
    }

    /// Get the raw `u32` value.
    pub const fn value(self) -> u32
    {
        self.0
    }

    /// Check if this slot is valid for x86-64 (must be 0-3).
    pub const fn is_valid_x86_64(self) -> bool
    {
        self.0 < 4
    }

    /// Check if this slot is valid for ARM64 (must be 0-15).
    pub const fn is_valid_arm64(self) -> bool
    {
        self.0 < 16
    }
}

impl From<u32> for BreakpointSlot
{
    fn from(value: u32) -> Self
    {
        Self(value)
    }
}

impl From<BreakpointSlot> for u32
{
    fn from(slot: BreakpointSlot) -> Self
    {
        slot.0
    }
}
