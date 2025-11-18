//! Memory address type.

use std::fmt;
use std::ops::{Add, Sub};

/// Strongly typed memory address
///
/// This wrapper around `u64` provides type safety when working with memory
/// addresses. It prevents accidentally mixing addresses with other `u64` values
/// (like sizes, counts, or other numeric types).
///
/// ## Why use a newtype?
///
/// - **Type safety**: Prevents accidentally passing a size where an address is expected
/// - **Self-documenting**: Makes it clear that a value represents a memory address
/// - **Future extensibility**: Can add address validation or methods later
///
/// ## Address Space
///
/// On 64-bit systems, addresses are 64-bit values. However, not all 64-bit values
/// are valid addresses. The actual addressable space depends on the platform:
///
/// - **macOS/Linux**: Typically 48-bit virtual addresses (can be extended to 57-bit)
/// - **Windows**: 48-bit virtual addresses
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::Address;
///
/// let addr = Address::from(0x1000);
/// let next_addr = addr + 0x100; // Add offset
/// assert_eq!(next_addr.value(), 0x1100);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(u64);

impl Address
{
    /// The null address (0x0)
    ///
    /// This is typically an invalid address on most systems, but can be used
    /// as a sentinel value or for initialization.
    pub const ZERO: Self = Address(0);

    /// Create a new address from a `u64` value
    ///
    /// This is equivalent to `Address::from(value)` but can be used in const contexts.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// const STACK_BASE: Address = Address::new(0x7fff00000000);
    /// ```
    pub const fn new(value: u64) -> Self
    {
        Address(value)
    }

    /// Get the raw `u64` value of this address
    ///
    /// This returns the underlying address value. Use this when you need to pass
    /// the address to platform-specific APIs that expect a `u64`.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.value(), 0x1000);
    /// ```
    pub const fn value(self) -> u64
    {
        self.0
    }

    /// Add an offset to this address, checking for overflow
    ///
    /// Returns `Some(new_address)` if the addition doesn't overflow, or `None` if it does.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.checked_add(0x100), Some(Address::from(0x1100)));
    /// assert_eq!(addr.checked_add(u64::MAX), None); // Overflow
    /// ```
    pub fn checked_add(self, offset: u64) -> Option<Self>
    {
        self.0.checked_add(offset).map(Address)
    }

    /// Subtract an offset from this address, checking for underflow
    ///
    /// Returns `Some(new_address)` if the subtraction doesn't underflow, or `None` if it does.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.checked_sub(0x100), Some(Address::from(0xf00)));
    /// assert_eq!(addr.checked_sub(u64::MAX), None); // Underflow
    /// ```
    pub fn checked_sub(self, offset: u64) -> Option<Self>
    {
        self.0.checked_sub(offset).map(Address)
    }

    /// Add an offset to this address, saturating at the maximum value
    ///
    /// If the addition would overflow, returns `Address::new(u64::MAX)` instead.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.saturating_add(0x100), Address::from(0x1100));
    /// assert_eq!(addr.saturating_add(u64::MAX), Address::new(u64::MAX)); // Saturates
    /// ```
    pub fn saturating_add(self, offset: u64) -> Self
    {
        Address(self.0.saturating_add(offset))
    }
}

impl From<u64> for Address
{
    fn from(value: u64) -> Self
    {
        Address(value)
    }
}

impl From<Address> for u64
{
    fn from(address: Address) -> Self
    {
        address.0
    }
}

impl fmt::Display for Address
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        write!(f, "0x{:016x}", self.0)
    }
}

impl Add<u64> for Address
{
    type Output = Address;

    fn add(self, rhs: u64) -> Self::Output
    {
        Address(self.0.wrapping_add(rhs))
    }
}

impl Sub<u64> for Address
{
    type Output = Address;

    fn sub(self, rhs: u64) -> Self::Output
    {
        Address(self.0.wrapping_sub(rhs))
    }
}
