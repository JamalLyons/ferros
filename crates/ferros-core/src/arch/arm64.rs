//! 64-bit ARM (AArch64).

use super::Arch;
use crate::backend::{BackendError, BackendResult};
use crate::types::{Address, Architecture, Registers};

/// Index of the frame pointer X29 in [`Registers::general`].
const FRAME_POINTER_INDEX: usize = 29;

/// Index of the link register X30 in [`Registers::general`].
const LINK_REGISTER_INDEX: usize = 30;

/// AArch64 architecture knowledge.
///
/// Pointer-authentication stripping depends on how many low address bits are
/// significant, which is an OS configuration; `Arm64` therefore holds
/// `addressable_bits`. [`Arm64::new`] and [`Default`] use
/// [`DEFAULT_ADDRESSABLE_BITS`](Self::DEFAULT_ADDRESSABLE_BITS); a platform with a
/// different user address-space size constructs [`Arm64::with_addressable_bits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64
{
    addressable_bits: u8,
}

impl Arm64
{
    /// 47-bit user address space, the configuration used on Apple Silicon.
    pub const DEFAULT_ADDRESSABLE_BITS: u8 = 47;

    /// `BRK #0` (0xD4200000), little-endian.
    pub const TRAP_INSTRUCTION: [u8; 4] = [0x00, 0x00, 0x20, 0xD4];

    /// Arm64 with [`DEFAULT_ADDRESSABLE_BITS`](Self::DEFAULT_ADDRESSABLE_BITS).
    #[must_use]
    pub const fn new() -> Self
    {
        Self {
            addressable_bits: Self::DEFAULT_ADDRESSABLE_BITS,
        }
    }

    /// Arm64 whose user pointers have `bits` significant bits.
    ///
    /// `bits = 64` disables pointer-authentication stripping.
    ///
    /// # Errors
    ///
    /// [`BackendError::Unsupported`] if `bits` is not in `1..=64`.
    pub fn with_addressable_bits(bits: u8) -> BackendResult<Self>
    {
        if (1..=64).contains(&bits) {
            Ok(Self { addressable_bits: bits })
        } else {
            Err(BackendError::Unsupported(format!(
                "arm64 addressable bits must be in 1..=64, got {bits}"
            )))
        }
    }

    /// Number of significant address bits in user pointers.
    #[must_use]
    pub const fn addressable_bits(&self) -> u8
    {
        self.addressable_bits
    }
}

impl Default for Arm64
{
    fn default() -> Self
    {
        Self::new()
    }
}

impl Arch for Arm64
{
    fn architecture(&self) -> Architecture
    {
        Architecture::Arm64
    }

    fn trap_instruction(&self) -> &'static [u8]
    {
        &Self::TRAP_INSTRUCTION
    }

    fn is_trap_instruction(&self, bytes: &[u8]) -> bool
    {
        // BRK #imm16: 1101 0100 001 imm16 000 00.
        bytes
            .try_into()
            .is_ok_and(|word| u32::from_le_bytes(word) & 0xffe0_001f == 0xd420_0000)
    }

    fn call_return_address(&self, pc: Address, instruction: &[u8]) -> Option<Address>
    {
        let word = u32::from_le_bytes(instruction.try_into().ok()?);
        // BL imm26, BLR Xn, the pointer-authenticated BLRAA/BLRAB Xn, Xm and
        // BLRAAZ/BLRABZ Xn (arm64e). B/BR, their authenticated forms and RET do not
        // create a return address.
        let bl = word & 0xfc00_0000 == 0x9400_0000;
        let blr = word & 0xffff_fc1f == 0xd63f_0000;
        let blra = word & 0xffff_f800 == 0xd73f_0800;
        let blraz = word & 0xffff_f81f == 0xd63f_081f;
        (bl || blr || blra || blraz).then(|| pc + 4)
    }

    fn pc(&self, registers: &Registers) -> Address
    {
        registers.pc
    }

    fn set_pc(&self, registers: &mut Registers, value: Address)
    {
        registers.pc = value;
    }

    fn sp(&self, registers: &Registers) -> Address
    {
        registers.sp
    }

    fn set_sp(&self, registers: &mut Registers, value: Address)
    {
        registers.sp = value;
    }

    fn fp(&self, registers: &Registers) -> Address
    {
        registers.fp
    }

    /// `Registers` holds x29 twice, as `fp` and as `general[29]`; both are updated so
    /// they never disagree.
    fn set_fp(&self, registers: &mut Registers, value: Address)
    {
        registers.fp = value;
        if let Some(slot) = registers.general.get_mut(FRAME_POINTER_INDEX) {
            *slot = value.value();
        }
    }

    fn link_register(&self, registers: &Registers) -> Option<Address>
    {
        registers.general.get(LINK_REGISTER_INDEX).copied().map(Address::new)
    }

    fn set_link_register(&self, registers: &mut Registers, value: Address) -> BackendResult<()>
    {
        let len = registers.general.len();
        let slot = registers
            .general
            .get_mut(LINK_REGISTER_INDEX)
            .ok_or_else(|| BackendError::Unsupported(format!("register set has {len} general registers; x30 is missing")))?;
        *slot = value.value();
        Ok(())
    }

    /// After `BRK` the reported pc is the address of the `BRK` itself: no adjustment.
    fn pc_after_trap(&self, reported_pc: Address) -> Address
    {
        reported_pc
    }

    /// Clears every bit at and above `addressable_bits`. Only valid for user-space
    /// addresses (kernel addresses have those bits set rather than cleared).
    fn strip_pointer_auth(&self, address: Address) -> Address
    {
        if self.addressable_bits >= 64 {
            return address;
        }
        let mask = (1u64 << self.addressable_bits) - 1;
        Address::new(address.value() & mask)
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn regs() -> Registers
    {
        let mut regs = Registers::new().with_arch(Architecture::Arm64);
        regs.general = vec![0; 31];
        regs
    }

    #[test]
    fn calls_have_a_return_address_but_other_branches_do_not()
    {
        let arch = Arm64::new();
        let pc = Address::new(0x1000);
        for word in [0x9400_0000_u32, 0x97ff_ffff, 0xd63f_0000, 0xd63f_03c0] {
            assert_eq!(arch.call_return_address(pc, &word.to_le_bytes()), Some(pc + 4));
        }
        for word in [0x1400_0000_u32, 0xd61f_0000, 0xd65f_03c0, 0xd420_0000] {
            assert_eq!(arch.call_return_address(pc, &word.to_le_bytes()), None);
        }
        assert_eq!(arch.call_return_address(pc, &[0x94]), None);
    }

    /// Issue #120 (4): the arm64e pointer-authenticated calls return to `pc + 4`; their
    /// branch-only forms (BRAA, BRAAZ, RETAA) are not calls.
    #[test]
    fn pointer_authenticated_calls_have_a_return_address()
    {
        let arch = Arm64::new();
        let pc = Address::new(0x1_0000_4000);
        let calls = [
            ("BLRAA x16, x17", 0xd73f_0a11_u32),
            ("BLRAB x8, sp", 0xd73f_0d1f),
            ("BLRAAZ x16", 0xd63f_0a1f),
            ("BLRABZ x1", 0xd63f_0c3f),
        ];
        for (name, word) in calls {
            assert_eq!(arch.call_return_address(pc, &word.to_le_bytes()), Some(pc + 4), "{name}");
        }
        let branches = [
            ("BRAA x16, x17", 0xd71f_0a11_u32),
            ("BRAAZ x16", 0xd61f_0a1f),
            ("RETAA", 0xd65f_0bff),
            ("BLR with a nonzero low field", 0xd63f_0201),
        ];
        for (name, word) in branches {
            assert_eq!(arch.call_return_address(pc, &word.to_le_bytes()), None, "{name}");
        }
    }

    #[test]
    fn trap_bytes_are_brk_0()
    {
        let arch = Arm64::new();
        assert_eq!(arch.trap_instruction(), &[0x00, 0x00, 0x20, 0xD4]);
        assert_eq!(u32::from_le_bytes(Arm64::TRAP_INSTRUCTION), 0xD420_0000);
        assert_eq!(arch.architecture(), Architecture::Arm64);
        assert_eq!(Arm64::default(), arch);
        assert_eq!(arch.addressable_bits(), 47);
    }

    #[test]
    fn any_brk_is_a_trap_instruction()
    {
        let arch = Arm64::new();
        let traps = [
            ("BRK #0", 0xd420_0000_u32),
            ("BRK #1", 0xd420_0020),
            ("BRK #0xf000", 0xd43e_0000),
            ("BRK #0xffff", 0xd43f_ffe0),
        ];
        for (name, word) in traps {
            assert!(arch.is_trap_instruction(&word.to_le_bytes()), "{name}");
        }
        let others = [
            ("NOP", 0xd503_201f_u32),
            ("HLT #0", 0xd440_0000),
            ("SVC #0x80", 0xd400_1001),
            ("BRK with a nonzero low field", 0xd420_0001),
            ("BRK with a nonzero op2 field", 0xd420_0004),
        ];
        for (name, word) in others {
            assert!(!arch.is_trap_instruction(&word.to_le_bytes()), "{name}");
        }
        assert!(!arch.is_trap_instruction(&[0x00, 0x00, 0x20]), "short read");
        assert!(!arch.is_trap_instruction(&[0x00, 0x00, 0x20, 0xd4, 0x00]), "long read");
    }

    #[test]
    fn accessors_round_trip()
    {
        let arch = Arm64::new();
        let mut regs = regs();
        arch.set_pc(&mut regs, Address::new(0x1000));
        arch.set_sp(&mut regs, Address::new(0x2000));
        arch.set_fp(&mut regs, Address::new(0x3000));
        arch.set_link_register(&mut regs, Address::new(0x4000)).expect("x30 present");
        assert_eq!(arch.pc(&regs), Address::new(0x1000));
        assert_eq!(arch.sp(&regs), Address::new(0x2000));
        assert_eq!(arch.fp(&regs), Address::new(0x3000));
        assert_eq!(arch.link_register(&regs), Some(Address::new(0x4000)));
        assert_eq!(regs.pc, Address::new(0x1000));
        assert_eq!(regs.general[30], 0x4000);
        assert_eq!(regs.general[29], 0x3000, "set_fp keeps general[29] in step with fp");
    }

    #[test]
    fn link_register_on_short_register_set()
    {
        let arch = Arm64::new();
        let mut regs = Registers::new().with_arch(Architecture::Arm64);
        regs.general = vec![7; 30];
        assert_eq!(arch.link_register(&regs), None);
        assert!(matches!(
            arch.set_link_register(&mut regs, Address::new(1)),
            Err(BackendError::Unsupported(_))
        ));
        assert_eq!(regs.general, vec![7; 30]);
    }

    #[test]
    fn pc_after_trap_is_identity()
    {
        let arch = Arm64::new();
        assert_eq!(arch.pc_after_trap(Address::new(0x1_0000_4000)), Address::new(0x1_0000_4000));
    }

    #[test]
    fn strip_pointer_auth_default_47_bits()
    {
        let arch = Arm64::new();
        assert_eq!(
            arch.strip_pointer_auth(Address::new(0xFFFF_8001_0000_4000)),
            Address::new(0x0000_0001_0000_4000)
        );
        assert_eq!(
            arch.strip_pointer_auth(Address::new(0x8012_3401_0000_4000)),
            Address::new(0x0000_3401_0000_4000)
        );
        // Bit 46 is kept, bit 47 is cleared.
        assert_eq!(
            arch.strip_pointer_auth(Address::new(0x0000_C000_0000_0000)),
            Address::new(0x0000_4000_0000_0000)
        );
        assert_eq!(
            arch.strip_pointer_auth(Address::new(0x1_0000_4000)),
            Address::new(0x1_0000_4000)
        );
    }

    #[test]
    fn strip_pointer_auth_custom_bits()
    {
        let arch = Arm64::with_addressable_bits(39).expect("valid");
        assert_eq!(
            arch.strip_pointer_auth(Address::new(0xFFFF_FF80_0000_1234)),
            Address::new(0x0000_0000_0000_1234)
        );
        let arch = Arm64::with_addressable_bits(64).expect("valid");
        assert_eq!(arch.strip_pointer_auth(Address::new(u64::MAX)), Address::new(u64::MAX));
        let arch = Arm64::with_addressable_bits(1).expect("valid");
        assert_eq!(arch.strip_pointer_auth(Address::new(0xFF)), Address::new(1));
    }

    #[test]
    fn addressable_bits_validation()
    {
        assert!(matches!(Arm64::with_addressable_bits(0), Err(BackendError::Unsupported(_))));
        assert!(matches!(Arm64::with_addressable_bits(65), Err(BackendError::Unsupported(_))));
    }
}
