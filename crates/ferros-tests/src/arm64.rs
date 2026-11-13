//! Small arm64 helpers for tests that check where a step or a breakpoint leaves the pc.

/// The arm64 `brk #0` the engine plants (`Arch::trap_instruction`), little endian.
pub const TRAP: [u8; 4] = [0x00, 0x00, 0x20, 0xD4];

/// The arm64 `nop`, little endian.
pub const NOP: [u8; 4] = [0x1F, 0x20, 0x03, 0xD5];

/// Where control goes after an arm64 instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow
{
    /// Not a branch: the next instruction.
    Next,
    /// `B` or `BL`: always this target.
    Direct(u64),
    /// `B.cond`, `CBZ`/`CBNZ`, `TBZ`/`TBNZ`: this target or the next instruction.
    Conditional(u64),
    /// A branch to a register (`BR`, `BLR`, `RET`, ...): anywhere.
    Indirect,
}

/// Sign-extend the low `bits` bits of `value`.
fn sign_extend(value: u32, bits: u32) -> i64
{
    let shift = 64 - bits;
    (i64::from(value) << shift) >> shift
}

/// Decode the control flow of the instruction `insn` at `pc`.
#[must_use]
pub fn flow(pc: u64, insn: u32) -> Flow
{
    let target = |offset: i64| pc.wrapping_add_signed(offset * 4);
    if insn & 0x7C00_0000 == 0x1400_0000 {
        Flow::Direct(target(sign_extend(insn & 0x03FF_FFFF, 26)))
    } else if insn & 0xFF00_0010 == 0x5400_0000 || insn & 0x7E00_0000 == 0x3400_0000 {
        Flow::Conditional(target(sign_extend((insn >> 5) & 0x7_FFFF, 19)))
    } else if insn & 0x7E00_0000 == 0x3600_0000 {
        Flow::Conditional(target(sign_extend((insn >> 5) & 0x3FFF, 14)))
    } else if insn & 0xFE00_0000 == 0xD600_0000 {
        Flow::Indirect
    } else {
        Flow::Next
    }
}

/// The amount `insn` subtracts from `sp` if it is `sub sp, sp, #imm{, lsl #12}` (the
/// usual first instruction of a function prologue), otherwise `None`.
#[must_use]
pub fn sp_decrement(insn: u32) -> Option<u64>
{
    if insn & 0xFF80_03FF != 0xD100_03FF {
        return None;
    }
    let imm = u64::from((insn >> 10) & 0xFFF);
    Some(if insn & (1 << 22) == 0 { imm } else { imm << 12 })
}

#[cfg(test)]
mod tests
{
    use super::{Flow, flow, sp_decrement};

    #[test]
    fn decodes_branches_and_prologues()
    {
        // bl -0x64 at 0x1000 (from `otool`-style encodings).
        assert_eq!(flow(0x1000, 0x97FF_FFE7), Flow::Direct(0x1000 - 0x64));
        // b +8
        assert_eq!(flow(0x1000, 0x1400_0002), Flow::Direct(0x1008));
        // ret
        assert_eq!(flow(0x1000, 0xD65F_03C0), Flow::Indirect);
        // nop
        assert_eq!(flow(0x1000, 0xD503_201F), Flow::Next);
        // sub sp, sp, #0x30
        assert_eq!(sp_decrement(0xD100_C3FF), Some(0x30));
        assert_eq!(sp_decrement(0xD503_201F), None);
    }
}
