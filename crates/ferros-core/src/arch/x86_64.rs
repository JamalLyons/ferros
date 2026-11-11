//! x86-64 placeholder.
//!
//! x86-64 is not supported yet: there is no [`Arch`](super::Arch) implementation and
//! [`for_architecture`](super::for_architecture) returns
//! [`BackendError::Unsupported`](crate::backend::BackendError::Unsupported) for it. This
//! module records the facts a future implementation needs.

/// Namespace for x86-64 constants; it does not implement [`Arch`](super::Arch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct X86_64;

impl X86_64
{
    /// `INT3`, the one-byte software-breakpoint instruction.
    ///
    /// Unlike arm64, after `INT3` executes the reported pc is one byte *past* the trap,
    /// so a future `pc_after_trap` subtracts [`TRAP_LENGTH`](Self::TRAP_LENGTH).
    pub const TRAP_INSTRUCTION: [u8; 1] = [0xCC];

    /// Length in bytes of [`TRAP_INSTRUCTION`](Self::TRAP_INSTRUCTION).
    pub const TRAP_LENGTH: u64 = 1;
}
