//! # Architecture knowledge, separate from the operating system
//!
//! An [`Arch`] answers CPU questions the portable engine needs (which bytes form a
//! software breakpoint, which register is the program counter, how the pc reported after
//! a trap relates to the trap address) without knowing anything about the OS. The
//! [`Backend`](crate::backend::Backend) answers the OS questions.
//!
//! Every implementation compiles on every host, so a debugger built for one machine can
//! reason about a target of another architecture.

#![forbid(unsafe_code)]

mod arm64;
mod x86_64;

pub use arm64::Arm64;
pub use x86_64::X86_64;

use crate::backend::{BackendError, BackendResult};
use crate::types::{Address, Architecture, Registers};

/// CPU architecture knowledge used by the engine.
///
/// All methods are pure computations over their arguments; implementations hold no
/// target state, so one value can be shared across threads.
pub trait Arch: Send + Sync + std::fmt::Debug
{
    /// The architecture this implementation describes.
    ///
    /// # Preconditions
    ///
    /// None.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn architecture(&self) -> Architecture;

    /// Bytes of the software-breakpoint instruction, in memory order.
    ///
    /// # Preconditions
    ///
    /// None.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn trap_instruction(&self) -> &'static [u8];

    /// Program counter held in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn pc(&self, registers: &Registers) -> Address;

    /// Set the program counter in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn set_pc(&self, registers: &mut Registers, value: Address);

    /// Stack pointer held in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn sp(&self, registers: &Registers) -> Address;

    /// Set the stack pointer in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn set_sp(&self, registers: &mut Registers, value: Address);

    /// Frame pointer held in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn fp(&self, registers: &Registers) -> Address;

    /// Set the frame pointer in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn set_fp(&self, registers: &mut Registers, value: Address);

    /// Return address held in the link register, if the architecture has one.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture.
    ///
    /// # Errors
    ///
    /// Infallible; returns `None` when the architecture has no link register or
    /// `registers` is too short to contain it.
    fn link_register(&self, registers: &Registers) -> Option<Address>;

    /// Set the link register in `registers`.
    ///
    /// # Preconditions
    ///
    /// `registers` belongs to this architecture and contains the link register.
    ///
    /// # Errors
    ///
    /// [`BackendError::Unsupported`] when the architecture has no link register or
    /// `registers` is too short to contain it; `registers` is then unchanged.
    fn set_link_register(&self, registers: &mut Registers, value: Address) -> BackendResult<()>;

    /// Map the pc the OS reports after a trap instruction executed to the address of the
    /// trap instruction itself.
    ///
    /// # Preconditions
    ///
    /// `reported_pc` comes from a [`StopEvent::Breakpoint`](crate::backend::StopEvent::Breakpoint).
    ///
    /// # Errors
    ///
    /// None; infallible (wrapping arithmetic).
    fn pc_after_trap(&self, reported_pc: Address) -> Address;

    /// Remove pointer-authentication bits from a code or data pointer.
    ///
    /// Architectures without pointer authentication return `address` unchanged.
    ///
    /// # Preconditions
    ///
    /// `address` is a user-space pointer.
    ///
    /// # Errors
    ///
    /// None; infallible.
    fn strip_pointer_auth(&self, address: Address) -> Address;
}

static DEFAULT_ARM64: Arm64 = Arm64::new();

/// Default [`Arch`] implementation for `architecture`.
///
/// # Errors
///
/// [`BackendError::Unsupported`] for [`Architecture::X86_64`] (not implemented yet) and
/// [`Architecture::Unknown`].
pub fn for_architecture(architecture: Architecture) -> BackendResult<&'static dyn Arch>
{
    match architecture {
        Architecture::Arm64 => Ok(&DEFAULT_ARM64),
        Architecture::X86_64 => Err(BackendError::Unsupported(
            "x86_64 architecture support is not implemented".into(),
        )),
        Architecture::Unknown(name) => Err(BackendError::Unsupported(format!("unknown architecture: {name}"))),
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn for_architecture_cases()
    {
        let arch = for_architecture(Architecture::Arm64).expect("arm64 supported");
        assert_eq!(arch.architecture(), Architecture::Arm64);
        assert_eq!(arch.trap_instruction(), &[0x00, 0x00, 0x20, 0xD4]);
        assert!(matches!(
            for_architecture(Architecture::X86_64),
            Err(BackendError::Unsupported(_))
        ));
        assert!(matches!(
            for_architecture(Architecture::Unknown("riscv64")),
            Err(BackendError::Unsupported(_))
        ));
    }
}
