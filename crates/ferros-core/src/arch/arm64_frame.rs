//! arm64 prologue and epilogue analysis for the stack unwinder.
//!
//! A format's unwind hint describes a function only once its frame is built
//! ([`UnwindHint::FrameRecord`](crate::objfile::UnwindHint::FrameRecord)) or only before
//! its epilogue releases the stack ([`UnwindHint::Leaf`](crate::objfile::UnwindHint::Leaf)). Between those points
//! the unwinder decodes the instructions around the pc of the innermost frame:
//!
//! - [`epilogue`]: from the pc forward to the `ret`, executing only the instructions that
//!   move `sp` or load `x29`, `x30` and the callee-saved registers, and following
//!   unconditional `b`. Straight-line code up to a `ret` runs next, so the registers at the
//!   `ret` are the caller's.
//! - [`prologue`]: from the function's first instruction forward to the pc, recording
//!   how far `sp` moved and where registers were stored, until an instruction sets `x29`
//!   to the new frame ([`Prologue::FrameBuilt`]).
//!
//! Both are conservative: an instruction that is not understood ([`decode`] returns
//! `None`), a conditional or indirect branch or a call (any branch, for [`prologue`]), or an instruction that writes `sp`, `x29` or `x30` in an
//! unmodelled way ends the analysis with `None`, and the unwinder keeps its other rules.
//!
//! Recognized frame instructions include `stp`/`ldp`/`str`/`ldr` with `sp` as base (offset,
//! pre-index and post-index), `sub`/`add sp, sp, #N`, `add x29, sp, #N` (`mov x29, sp`),
//! `mov sp, x29` / `sub sp, x29, #N`, `ret`/`retaa`/`retab`, and hints such as `pacibsp`
//! and `autibsp`. Ordinary data processing and loads into `x0`-`x28` are neutral.

#![forbid(unsafe_code)]

use super::Arm64;

/// `x29`, the frame pointer.
const FP: u8 = 29;
/// `x30`, the link register.
const LR: u8 = 30;
/// Register field value 31: `sp` as a base or an add/sub destination, else the zero
/// register.
const SP: u8 = 31;

/// How a load or store forms its address from the base register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Indexing
{
    /// `[sp, #offset]`: no writeback.
    Offset,
    /// `[sp, #offset]!`: the address is `sp + offset`, written back to `sp`.
    PreIndex,
    /// `[sp], #offset`: the address is `sp`; then `sp += offset`.
    PostIndex,
}

/// The effect of one instruction on a stack frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameOp
{
    /// Leaves `sp`, `x29`, `x30` and control flow alone; may overwrite the general
    /// registers listed (`x0`-`x28`).
    Neutral
    {
        /// Overwritten general registers.
        writes: [Option<u8>; 3],
    },
    /// `add`/`sub sp, sp, #imm`: `sp += delta`.
    AdjustSp(i64),
    /// `mov sp, x29`, `add`/`sub sp, x29, #imm`: `sp = x29 + delta`.
    SpFromFp(i64),
    /// `mov x29, sp`, `add`/`sub x29, sp, #imm`: `x29 = sp + delta`.
    FpFromSp(i64),
    /// A load or store with `sp` as base.
    SpAccess
    {
        /// Load (`true`) or store.
        load: bool,
        /// The general registers transferred, in memory order (`None` for the zero
        /// register or a SIMD/FP register).
        registers: [Option<u8>; 2],
        /// Registers transferred: 2 for a pair, 1 for a single register (whose second
        /// `registers` entry is unused).
        count: u8,
        /// Bytes per register.
        size: u8,
        /// Signed byte offset.
        offset: i64,
        /// Addressing mode.
        indexing: Indexing,
    },
    /// `ret` (through `x30`), `retaa`, `retab`.
    Return,
    /// `b label`: an unconditional branch by this many bytes.
    Branch(i64),
}

fn bits(insn: u32, low: u32, count: u32) -> u32
{
    (insn >> low) & ((1 << count) - 1)
}

fn register(insn: u32, low: u32) -> u8
{
    // A five-bit field always fits.
    u8::try_from(bits(insn, low, 5)).unwrap_or(SP)
}

fn sign_extend(value: u32, width: u32) -> i64
{
    let shift = 64 - width;
    (i64::from(value) << shift) >> shift
}

/// `writes` for a destination register field: `None` if it is `x29`/`x30` (not neutral),
/// no write for the zero register (31).
fn neutral_write(rd: u8) -> Option<FrameOp>
{
    match rd {
        FP | LR => None,
        SP => Some(FrameOp::Neutral { writes: [None; 3] }),
        rd => Some(FrameOp::Neutral {
            writes: [Some(rd), None, None],
        }),
    }
}

/// Decode `insn` for prologue and epilogue analysis, or `None` when it is not understood
/// or affects the frame in a way the analysis does not model (branches, calls, traps,
/// other writes of `sp`, `x29` or `x30`).
#[must_use]
pub(crate) fn decode(insn: u32) -> Option<FrameOp>
{
    match bits(insn, 25, 4) {
        // Data processing, immediate.
        0b1000 | 0b1001 => decode_immediate(insn),
        // Branches, exceptions, system.
        0b1010 | 0b1011 => decode_branch_or_system(insn),
        // Loads and stores.
        0b0100 | 0b0110 | 0b1100 | 0b1110 => decode_load_store(insn),
        // Data processing, register.
        0b0101 | 0b1101 => decode_register(insn),
        // SIMD and floating point data processing, and unallocated: not modelled.
        _ => None,
    }
}

fn decode_immediate(insn: u32) -> Option<FrameOp>
{
    let rd = register(insn, 0);
    let rn = register(insn, 5);
    match bits(insn, 23, 6) {
        // Add/subtract (immediate).
        0b10_0010 => {
            let sf = bits(insn, 31, 1) == 1;
            let subtract = bits(insn, 30, 1) == 1;
            let set_flags = bits(insn, 29, 1) == 1;
            let shift = if bits(insn, 22, 1) == 1 { 12 } else { 0 };
            let imm = i64::from(bits(insn, 10, 12)) << shift;
            let delta = if subtract { -imm } else { imm };
            if set_flags {
                // `cmp`/`cmn` write the zero register.
                return neutral_write(rd);
            }
            match (rd, rn) {
                (SP, SP) if sf => Some(FrameOp::AdjustSp(delta)),
                (SP, FP) if sf => Some(FrameOp::SpFromFp(delta)),
                (FP, SP) if sf => Some(FrameOp::FpFromSp(delta)),
                (SP | FP | LR, _) => None,
                _ => neutral_write(rd),
            }
        }
        // Logical (immediate): destination 31 is `sp` unless the flags are set.
        0b10_0100 => {
            if rd == SP && bits(insn, 29, 2) != 0b11 {
                return None;
            }
            neutral_write(rd)
        }
        // Move wide, bitfield, extract.
        0b10_0101..=0b10_0111 => neutral_write(rd),
        // `adr`, `adrp`.
        0b10_0000 | 0b10_0001 => neutral_write(rd),
        _ => None,
    }
}

fn decode_branch_or_system(insn: u32) -> Option<FrameOp>
{
    // `b imm26`.
    if insn & 0xFC00_0000 == 0x1400_0000 {
        return Some(FrameOp::Branch(sign_extend(bits(insn, 0, 26), 26) * 4));
    }
    // `ret Xn`: only through x30 returns to the caller.
    if insn & 0xFFFF_FC1F == 0xD65F_0000 {
        return (register(insn, 5) == LR).then_some(FrameOp::Return);
    }
    // `retaa`, `retab`.
    if insn == 0xD65F_0BFF || insn == 0xD65F_0FFF {
        return Some(FrameOp::Return);
    }
    // Hints: `nop`, `pacibsp`, `autibsp`, `paciasp`, `autiasp`, `bti`, ... They change
    // at most the pointer-authentication bits of `x30`, which the unwinder strips.
    if insn & 0xFFFF_F01F == 0xD503_201F {
        return Some(FrameOp::Neutral { writes: [None; 3] });
    }
    None
}

fn decode_register(insn: u32) -> Option<FrameOp>
{
    let rd = register(insn, 0);
    // Add/subtract (extended register) without flags writes `sp` for register 31.
    if bits(insn, 24, 5) == 0b0_1011 && bits(insn, 21, 1) == 1 && bits(insn, 29, 1) == 0 && rd == SP {
        return None;
    }
    neutral_write(rd)
}

fn decode_load_store(insn: u32) -> Option<FrameOp>
{
    match bits(insn, 27, 3) {
        0b101 if bits(insn, 25, 1) == 0 => decode_pair(insn),
        0b111 => decode_single(insn),
        // Load literal.
        0b011 if bits(insn, 24, 2) == 0 => {
            let simd = bits(insn, 26, 1) == 1;
            if simd || bits(insn, 30, 2) == 0b11 {
                // SIMD/FP destination, or `prfm`.
                Some(FrameOp::Neutral { writes: [None; 3] })
            } else {
                neutral_write(register(insn, 0))
            }
        }
        // Exclusives, atomics, SIMD structures: not modelled.
        _ => None,
    }
}

/// `stp`/`ldp`/`stnp`/`ldnp`/`ldpsw`, general or SIMD/FP.
fn decode_pair(insn: u32) -> Option<FrameOp>
{
    let simd = bits(insn, 26, 1) == 1;
    let load = bits(insn, 22, 1) == 1;
    let opc = bits(insn, 30, 2);
    let size: u8 = if simd {
        match opc {
            0 => 4,
            1 => 8,
            2 => 16,
            _ => return None,
        }
    } else {
        match opc {
            0 => 4,
            1 if load => 4,
            2 => 8,
            _ => return None,
        }
    };
    let indexing = match bits(insn, 23, 2) {
        0 | 2 => Indexing::Offset,
        1 => Indexing::PostIndex,
        _ => Indexing::PreIndex,
    };
    let offset = sign_extend(bits(insn, 15, 7), 7) * i64::from(size);
    let (rt, rt2) = (register(insn, 0), register(insn, 10));
    let registers = if simd { [None, None] } else { [Some(rt), Some(rt2)] };
    if load && !simd && rt == rt2 {
        return None;
    }
    access(load, registers, 2, size, register(insn, 5), offset, indexing)
}

/// `str`/`ldr` and their byte, half-word, signed, unscaled and SIMD/FP forms.
fn decode_single(insn: u32) -> Option<FrameOp>
{
    let simd = bits(insn, 26, 1) == 1;
    let opc = bits(insn, 22, 2);
    let size_bits = bits(insn, 30, 2);
    let (size, load) = if simd {
        let size = if size_bits == 0 && opc & 0b10 != 0 {
            16
        } else {
            1 << size_bits
        };
        (size, opc & 1 == 1)
    } else {
        match (size_bits, opc) {
            (3, 2) => return Some(FrameOp::Neutral { writes: [None; 3] }), // `prfm`
            (3, 3) => return None,
            (_, 0) => (1 << size_bits, false),
            _ => (1 << size_bits, true),
        }
    };
    let rt = register(insn, 0);
    let registers = if simd { [None, None] } else { [Some(rt), None] };
    let rn = register(insn, 5);
    if bits(insn, 24, 2) == 0b01 {
        let offset = i64::from(bits(insn, 10, 12)) * i64::from(size);
        return access(load, registers, 1, size, rn, offset, Indexing::Offset);
    }
    if bits(insn, 24, 2) != 0 {
        return None;
    }
    let offset = sign_extend(bits(insn, 12, 9), 9);
    match (bits(insn, 21, 1), bits(insn, 10, 2)) {
        (0, 0b00 | 0b10) => access(load, registers, 1, size, rn, offset, Indexing::Offset),
        (0, 0b01) => access(load, registers, 1, size, rn, offset, Indexing::PostIndex),
        (0, _) => access(load, registers, 1, size, rn, offset, Indexing::PreIndex),
        // Register offset: the address is not known.
        (1, 0b10) if rn != SP => {
            if load && !simd {
                neutral_write(rt)
            } else {
                Some(FrameOp::Neutral { writes: [None; 3] })
            }
        }
        _ => None,
    }
}

/// A load or store through `rn`.
fn access(
    load: bool,
    registers: [Option<u8>; 2],
    count: u8,
    size: u8,
    rn: u8,
    offset: i64,
    indexing: Indexing,
) -> Option<FrameOp>
{
    // Register 31 as a transfer register is the zero register.
    let registers = registers.map(|r| r.filter(|&r| r != SP));
    let frame_register = registers.iter().flatten().any(|&r| r == FP || r == LR);
    if rn == SP {
        if frame_register && size != 8 {
            return None;
        }
        return Some(FrameOp::SpAccess {
            load,
            registers,
            count,
            size,
            offset,
            indexing,
        });
    }
    let writeback = indexing != Indexing::Offset;
    if (writeback && (rn == FP || rn == LR)) || (load && frame_register) {
        return None;
    }
    let mut writes = [None; 3];
    if load {
        writes[0] = registers[0];
        writes[1] = registers[1];
    }
    if writeback {
        writes[2] = Some(rn);
    }
    Some(FrameOp::Neutral { writes })
}

/// Registers of one frame during the analysis: `x0`-`x30`, `sp`, and which of `x0`-`x30`
/// hold known values (bit `n` for `xn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameRegisters
{
    /// `x0`-`x30`.
    pub(crate) x: [u64; 31],
    /// The stack pointer.
    pub(crate) sp: u64,
    /// Bit `n` set: `x[n]` is known.
    pub(crate) known: u32,
}

/// Most instructions [`epilogue`] reads before giving up.
pub(crate) const EPILOGUE_LIMIT: u64 = 32;
/// Most instructions [`prologue`] reads before giving up.
pub(crate) const PROLOGUE_LIMIT: u64 = 64;

fn bit(register: u8) -> u32
{
    1 << register
}

/// Execute the straight-line code from `pc` to its `ret`, and return the registers at
/// that `ret`: the caller's `sp`, `x29`, the return address in `x30`, and the callee-saved
/// registers the epilogue reloads. Registers the code overwrites otherwise become unknown.
///
/// `read_code` reads one instruction word, `read_stack` one 64-bit word of the stack.
/// `None` when an instruction is not understood (see [`decode`]), memory cannot be read,
/// a needed register is unknown, or no `ret` comes within [`EPILOGUE_LIMIT`] instructions.
pub(crate) fn epilogue(
    pc: u64,
    registers: &FrameRegisters,
    read_code: impl Fn(u64) -> Option<u32>,
    read_stack: impl Fn(u64) -> Option<u64>,
) -> Option<FrameRegisters>
{
    let mut state = registers.clone();
    // Stack words the simulated code stored: the value, or `None` when it is not known
    // (a 32-bit or SIMD store, or an unknown register).
    let mut stored: Vec<(u64, Option<u64>)> = Vec::new();
    let mut at = pc;
    for _ in 0..EPILOGUE_LIMIT {
        let insn = read_code(at)?;
        let mut next = at.checked_add(4)?;
        match decode(insn)? {
            // Code after an unconditional branch runs next; the limit ends any loop.
            FrameOp::Branch(offset) => next = at.checked_add_signed(offset)?,
            FrameOp::Neutral { writes } => {
                for register in writes.into_iter().flatten() {
                    state.known &= !bit(register);
                }
            }
            FrameOp::AdjustSp(delta) => state.sp = state.sp.checked_add_signed(delta)?,
            FrameOp::SpFromFp(delta) => {
                if state.known & bit(FP) == 0 {
                    return None;
                }
                state.sp = state.x[usize::from(FP)].checked_add_signed(delta)?;
            }
            FrameOp::FpFromSp(delta) => {
                state.x[usize::from(FP)] = state.sp.checked_add_signed(delta)?;
                state.known |= bit(FP);
            }
            FrameOp::SpAccess {
                load,
                registers,
                count,
                size,
                offset,
                indexing,
            } => {
                let shifted = state.sp.checked_add_signed(offset)?;
                let address = if indexing == Indexing::PostIndex { state.sp } else { shifted };
                let size = u64::from(size);
                for (slot, register) in (0u64..).zip(registers).take(usize::from(count)) {
                    let at = address.checked_add(slot * size)?;
                    if load {
                        let Some(register) = register else {
                            continue;
                        };
                        // A caller-saved register's value is not needed: no read.
                        let value = if size == 8 && register >= Arm64::FIRST_CALLEE_SAVED_INDEX {
                            match stored.iter().rev().find(|(word, _)| *word == at) {
                                Some((_, value)) => *value,
                                None if stored.iter().any(|(word, _)| word.abs_diff(at) < 8) => None,
                                None => Some(read_stack(at)?),
                            }
                        } else {
                            None
                        };
                        match value {
                            Some(value) => {
                                state.x[usize::from(register)] = value;
                                state.known |= bit(register);
                            }
                            None => state.known &= !bit(register),
                        }
                    } else {
                        let value = register
                            .filter(|&r| size == 8 && state.known & bit(r) != 0)
                            .map(|r| state.x[usize::from(r)]);
                        // A store of `size` bytes; record every word it touches.
                        let mut word = at;
                        while word < at.checked_add(size)? {
                            stored.push((word, if size == 8 { value } else { None }));
                            word = word.checked_add(8)?;
                        }
                    }
                }
                if indexing != Indexing::Offset {
                    state.sp = shifted;
                }
            }
            FrameOp::Return => return Some(state),
        }
        at = next;
    }
    None
}

/// What [`prologue`] found between a function's first instruction and the pc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prologue
{
    /// An instruction set `x29` to the new frame: the frame record is built.
    FrameBuilt,
    /// The frame is not built yet: the caller's registers.
    Caller(Box<FrameRegisters>),
}

/// Follow the instructions from the function's first instruction `start` up to (not
/// including) `pc`, the pc of the innermost frame, tracking the stack allocated below the
/// caller's `sp` and where registers were stored, and return the caller's registers: `sp`
/// plus the allocation, and `x29`, `x30` and `x19`-`x28` from their save slots when they
/// were stored, else their current values. A register overwritten before it was stored
/// becomes unknown.
///
/// `None` when the code is not a straight line of understood instructions (any load from
/// the stack, branch or `ret` before the pc), memory cannot be read, `pc` is not within
/// [`PROLOGUE_LIMIT`] instructions after `start`, or a store lands at or above the
/// caller's `sp`.
pub(crate) fn prologue(
    start: u64,
    pc: u64,
    registers: &FrameRegisters,
    read_code: impl Fn(u64) -> Option<u32>,
    read_stack: impl Fn(u64) -> Option<u64>,
) -> Option<Prologue>
{
    let distance = pc.checked_sub(start)?;
    if distance % 4 != 0 || distance / 4 > PROLOGUE_LIMIT {
        return None;
    }
    // Bytes allocated below the caller's sp (the CFA).
    let mut allocated: u64 = 0;
    // Save slot of each register, as bytes below the CFA.
    let mut saved: [Option<u64>; 31] = [None; 31];
    let mut clobbered: u32 = 0;
    for index in 0..distance / 4 {
        let insn = read_code(start.checked_add(index * 4)?)?;
        match decode(insn)? {
            FrameOp::Neutral { writes } => {
                for register in writes.into_iter().flatten() {
                    if saved[usize::from(register)].is_none() {
                        clobbered |= bit(register);
                    }
                }
            }
            FrameOp::AdjustSp(delta) => allocated = allocated.checked_add_signed(delta.checked_neg()?)?,
            FrameOp::FpFromSp(_) => return Some(Prologue::FrameBuilt),
            FrameOp::SpAccess {
                load: true,
                registers,
                indexing: Indexing::Offset,
                ..
            } => {
                // Reloading a spilled argument is harmless; reloading a register the
                // caller needs back is not a prologue.
                for register in registers.into_iter().flatten() {
                    if register >= Arm64::FIRST_CALLEE_SAVED_INDEX {
                        return None;
                    }
                    if saved[usize::from(register)].is_none() {
                        clobbered |= bit(register);
                    }
                }
            }
            FrameOp::SpFromFp(_) | FrameOp::Return | FrameOp::Branch(_) | FrameOp::SpAccess { load: true, .. } => {
                return None;
            }
            FrameOp::SpAccess {
                load: false,
                registers,
                count,
                size,
                offset,
                indexing,
            } => {
                let negated = offset.checked_neg()?;
                let shifted = allocated.checked_add_signed(negated)?;
                // The store's address as bytes below the CFA.
                let below = if indexing == Indexing::PostIndex { allocated } else { shifted };
                for (slot, register) in (0u64..).zip(registers).take(usize::from(count)) {
                    let slot_below = below.checked_sub(slot * u64::from(size)).filter(|&b| b > 0)?;
                    let Some(register) = register else {
                        continue;
                    };
                    let index = usize::from(register);
                    // Only callee-saved registers and x30 are saved for the caller; a
                    // spilled argument register says nothing about the caller's value.
                    if register >= Arm64::FIRST_CALLEE_SAVED_INDEX
                        && size == 8
                        && saved[index].is_none()
                        && clobbered & bit(register) == 0
                    {
                        saved[index] = Some(slot_below);
                    }
                }
                if indexing != Indexing::Offset {
                    allocated = shifted;
                }
            }
        }
    }
    let cfa = registers.sp.checked_add(allocated)?;
    let mut caller = registers.clone();
    caller.sp = cfa;
    for (register, slot) in (0u8..).zip(saved) {
        if let Some(below) = slot {
            caller.x[usize::from(register)] = read_stack(cfa.checked_sub(below)?)?;
            caller.known |= bit(register);
        }
    }
    caller.known &= !(clobbered & !saved_mask(&saved));
    Some(Prologue::Caller(Box::new(caller)))
}

fn saved_mask(saved: &[Option<u64>; 31]) -> u32
{
    (0u8..)
        .zip(saved)
        .filter(|(_, slot)| slot.is_some())
        .fold(0, |mask, (register, _)| mask | bit(register))
}

#[cfg(test)]
mod tests
{
    use std::collections::HashMap;

    use super::*;

    // Encodings from `clang -arch arm64 -c` and `objdump -d`.
    const SUB_SP_0X20: u32 = 0xD100_83FF;
    const STP_FP_LR_SP_0X10: u32 = 0xA901_7BFD;
    const ADD_FP_SP_0X10: u32 = 0x9100_43FD;
    const STP_FP_LR_PRE_M0X10: u32 = 0xA9BF_7BFD;
    const MOV_FP_SP: u32 = 0x9100_03FD;
    const LDP_FP_LR_SP_0X10: u32 = 0xA941_7BFD;
    const ADD_SP_0X20: u32 = 0x9100_83FF;
    const LDP_FP_LR_POST_0X10: u32 = 0xA8C1_7BFD;
    const RET: u32 = 0xD65F_03C0;
    const RETAB: u32 = 0xD65F_0FFF;
    const RETAA: u32 = 0xD65F_0BFF;
    const PACIBSP: u32 = 0xD503_237F;
    const AUTIBSP: u32 = 0xD503_23FF;
    const NOP: u32 = 0xD503_201F;
    const STP_X20_X19_PRE_M0X20: u32 = 0xA9BE_4FF4;
    const LDP_X20_X19_POST_0X20: u32 = 0xA8C2_4FF4;
    const STP_D9_D8_PRE_M0X10: u32 = 0x6DBF_23E9;
    const LDP_D9_D8_POST_0X10: u32 = 0x6CC1_23E9;
    const STR_X19_PRE_M0X10: u32 = 0xF81F_0FF3;
    const LDR_X19_POST_0X10: u32 = 0xF841_07F3;
    const LDR_X19_SP_8: u32 = 0xF940_07F3;
    const STR_X19_SP_8: u32 = 0xF900_07F3;
    const SUB_SP_FP_0X10: u32 = 0xD100_43BF;
    const MOV_SP_FP: u32 = 0x9100_03BF;
    const SUB_SP_0X1000: u32 = 0xD140_07FF;
    const B: u32 = 0x1400_0000;
    const BR_X16: u32 = 0xD61F_0200;
    const BLR_X8: u32 = 0xD63F_0100;
    const BL: u32 = 0x9400_0000;
    const RET_X1: u32 = 0xD65F_0020;
    const MOV_X8_X0: u32 = 0xAA00_03E8;
    const LDR_W0_SP_0XC: u32 = 0xB940_0FE0;
    const ADD_FP_FP_0X10: u32 = 0x9100_43BD;
    const BRK_0: u32 = 0xD420_0000;
    const CMP_X0_8: u32 = 0xF100_201F;
    const ADRP_X0: u32 = 0x9000_0000;

    fn neutral(writes: &[u8]) -> Option<FrameOp>
    {
        let mut array = [None; 3];
        for (slot, &register) in array.iter_mut().zip(writes) {
            *slot = Some(register);
        }
        Some(FrameOp::Neutral { writes: array })
    }

    fn access(load: bool, registers: [Option<u8>; 2], size: u8, offset: i64, indexing: Indexing) -> Option<FrameOp>
    {
        // The tests' single-register forms name one general register; the SIMD pairs name none.
        let count = if registers[1].is_some() || registers[0].is_none() {
            2
        } else {
            1
        };
        Some(FrameOp::SpAccess {
            load,
            registers,
            count,
            size,
            offset,
            indexing,
        })
    }

    #[test]
    fn decodes_frame_setup_and_teardown()
    {
        assert_eq!(decode(SUB_SP_0X20), Some(FrameOp::AdjustSp(-0x20)));
        assert_eq!(decode(ADD_SP_0X20), Some(FrameOp::AdjustSp(0x20)));
        assert_eq!(decode(SUB_SP_0X1000), Some(FrameOp::AdjustSp(-0x1000)));
        assert_eq!(decode(ADD_FP_SP_0X10), Some(FrameOp::FpFromSp(0x10)));
        assert_eq!(decode(MOV_FP_SP), Some(FrameOp::FpFromSp(0)));
        assert_eq!(decode(MOV_SP_FP), Some(FrameOp::SpFromFp(0)));
        assert_eq!(decode(SUB_SP_FP_0X10), Some(FrameOp::SpFromFp(-0x10)));
        let pair = [Some(29), Some(30)];
        assert_eq!(decode(STP_FP_LR_SP_0X10), access(false, pair, 8, 0x10, Indexing::Offset));
        assert_eq!(decode(STP_FP_LR_PRE_M0X10), access(false, pair, 8, -0x10, Indexing::PreIndex));
        assert_eq!(decode(LDP_FP_LR_SP_0X10), access(true, pair, 8, 0x10, Indexing::Offset));
        assert_eq!(decode(LDP_FP_LR_POST_0X10), access(true, pair, 8, 0x10, Indexing::PostIndex));
        let callee = [Some(20), Some(19)];
        assert_eq!(
            decode(STP_X20_X19_PRE_M0X20),
            access(false, callee, 8, -0x20, Indexing::PreIndex)
        );
        assert_eq!(
            decode(LDP_X20_X19_POST_0X20),
            access(true, callee, 8, 0x20, Indexing::PostIndex)
        );
        assert_eq!(
            decode(STP_D9_D8_PRE_M0X10),
            access(false, [None, None], 8, -0x10, Indexing::PreIndex)
        );
        assert_eq!(
            decode(LDP_D9_D8_POST_0X10),
            access(true, [None, None], 8, 0x10, Indexing::PostIndex)
        );
        let x19 = [Some(19), None];
        assert_eq!(decode(STR_X19_PRE_M0X10), access(false, x19, 8, -0x10, Indexing::PreIndex));
        assert_eq!(decode(LDR_X19_POST_0X10), access(true, x19, 8, 0x10, Indexing::PostIndex));
        assert_eq!(decode(LDR_X19_SP_8), access(true, x19, 8, 8, Indexing::Offset));
        assert_eq!(decode(STR_X19_SP_8), access(false, x19, 8, 8, Indexing::Offset));
        assert_eq!(decode(LDR_W0_SP_0XC), access(true, [Some(0), None], 4, 0xc, Indexing::Offset));
    }

    #[test]
    fn decodes_returns_and_hints()
    {
        for insn in [RET, RETAA, RETAB] {
            assert_eq!(decode(insn), Some(FrameOp::Return), "{insn:#x}");
        }
        for insn in [PACIBSP, AUTIBSP, NOP] {
            assert_eq!(decode(insn), neutral(&[]), "{insn:#x}");
        }
    }

    #[test]
    fn neutral_instructions_report_their_destination()
    {
        assert_eq!(decode(MOV_X8_X0), neutral(&[8]));
        assert_eq!(decode(CMP_X0_8), neutral(&[]));
        assert_eq!(decode(ADRP_X0), neutral(&[0]));
    }

    #[test]
    fn control_flow_and_unmodelled_frame_writes_are_not_understood()
    {
        assert_eq!(decode(B), Some(FrameOp::Branch(0)));
        assert_eq!(decode(0x1400_0002), Some(FrameOp::Branch(8)));
        assert_eq!(decode(0x17FF_FFFF), Some(FrameOp::Branch(-4)));
        for insn in [BL, BR_X16, BLR_X8, RET_X1, BRK_0, ADD_FP_FP_0X10] {
            assert_eq!(decode(insn), None, "{insn:#x}");
        }
    }

    /// Code at 0x1000, one instruction per word; the stack as 64-bit words.
    struct Machine
    {
        code: Vec<u32>,
        stack: HashMap<u64, u64>,
    }

    const CODE: u64 = 0x1000;

    impl Machine
    {
        fn new(code: &[u32]) -> Self
        {
            Self {
                code: code.to_vec(),
                stack: HashMap::new(),
            }
        }

        fn read_code(&self) -> impl Fn(u64) -> Option<u32> + '_
        {
            |address| {
                let index = usize::try_from(address.checked_sub(CODE)? / 4).ok()?;
                self.code.get(index).copied()
            }
        }

        fn read_stack(&self) -> impl Fn(u64) -> Option<u64> + '_
        {
            |address| self.stack.get(&address).copied()
        }

        fn epilogue(&self, index: u64, registers: &FrameRegisters) -> Option<FrameRegisters>
        {
            epilogue(CODE + index * 4, registers, self.read_code(), self.read_stack())
        }

        fn prologue(&self, index: u64, registers: &FrameRegisters) -> Option<Prologue>
        {
            prologue(CODE, CODE + index * 4, registers, self.read_code(), self.read_stack())
        }
    }

    fn registers(sp: u64, fp: u64, lr: u64) -> FrameRegisters
    {
        let mut x = [0; 31];
        x[19] = 0x1919;
        x[29] = fp;
        x[30] = lr;
        FrameRegisters {
            x,
            sp,
            known: (1 << 31) - 1,
        }
    }

    fn caller(state: Option<Prologue>) -> FrameRegisters
    {
        match state {
            Some(Prologue::Caller(caller)) => *caller,
            other => panic!("expected the caller, got {other:?}"),
        }
    }

    /// `sub sp, sp, #0x20; stp x29, x30, [sp, #0x10]; add x29, sp, #0x10; ...;
    /// ldp x29, x30, [sp, #0x10]; add sp, sp, #0x20; ret` (rustc debug).
    fn rustc_frame() -> Machine
    {
        Machine::new(&[
            SUB_SP_0X20,
            STP_FP_LR_SP_0X10,
            ADD_FP_SP_0X10,
            MOV_X8_X0,
            LDP_FP_LR_SP_0X10,
            ADD_SP_0X20,
            RET,
        ])
    }

    #[test]
    fn prologue_before_the_frame_record()
    {
        let mut machine = rustc_frame();
        // After `sub sp`: nothing stored yet.
        let state = caller(machine.prologue(1, &registers(0xfe0, 0x2000, 0x4444)));
        assert_eq!((state.sp, state.x[29], state.x[30]), (0x1000, 0x2000, 0x4444));
        // After the `stp`: x29 and x30 come from their slots.
        machine.stack.insert(0xff0, 0x2000);
        machine.stack.insert(0xff8, 0x5555);
        let state = caller(machine.prologue(2, &registers(0xfe0, 0x2000, 0x9999)));
        assert_eq!((state.sp, state.x[29], state.x[30]), (0x1000, 0x2000, 0x5555));
        // After `add x29`: the frame-pointer chain applies.
        assert_eq!(machine.prologue(3, &registers(0xfe0, 0xff0, 0)), Some(Prologue::FrameBuilt));
    }

    #[test]
    fn prologue_with_pre_index_push_and_pointer_authentication()
    {
        let mut machine = Machine::new(&[PACIBSP, STP_FP_LR_PRE_M0X10, MOV_FP_SP]);
        machine.stack.insert(0xff0, 0x2000);
        machine.stack.insert(0xff8, 0x8000_0000_0000_5555);
        let state = caller(machine.prologue(1, &registers(0x1000, 0x2000, 0x5555)));
        assert_eq!(state.sp, 0x1000, "pacibsp moves nothing");
        let state = caller(machine.prologue(2, &registers(0xff0, 0x2000, 0)));
        assert_eq!((state.sp, state.x[30]), (0x1000, 0x8000_0000_0000_5555));
        assert_eq!(machine.prologue(3, &registers(0xff0, 0xff0, 0)), Some(Prologue::FrameBuilt));
    }

    #[test]
    fn prologue_restores_saved_callee_registers_and_forgets_clobbered_ones()
    {
        let mut machine = Machine::new(&[MOV_X8_X0, STP_X20_X19_PRE_M0X20, STP_FP_LR_SP_0X10, ADD_FP_SP_0X10]);
        machine.stack.insert(0xfe0, 0x2020);
        machine.stack.insert(0xfe8, 0x1919_0000);
        let state = caller(machine.prologue(2, &registers(0xfe0, 0x2000, 0x4444)));
        assert_eq!(state.sp, 0x1000);
        assert_eq!((state.x[19], state.x[20], state.x[30]), (0x1919_0000, 0x2020, 0x4444));
        assert_eq!(state.known & (1 << 8), 0, "x8 was overwritten");
    }

    #[test]
    fn prologue_gives_up_on_control_flow_and_far_pcs()
    {
        let machine = Machine::new(&[MOV_X8_X0, BL, SUB_SP_0X20]);
        assert!(machine.prologue(1, &registers(0x1000, 0, 0)).is_some());
        assert_eq!(machine.prologue(3, &registers(0x1000, 0, 0)), None, "a call before the pc");
        let machine = Machine::new(&[MOV_X8_X0, RET, SUB_SP_0X20]);
        assert_eq!(machine.prologue(3, &registers(0x1000, 0, 0)), None, "a ret before the pc");
        let far = Machine::new(&vec![NOP; 100]);
        assert_eq!(far.prologue(80, &registers(0x1000, 0, 0)), None);
        assert_eq!(
            prologue(CODE, CODE + 2, &registers(0x1000, 0, 0), far.read_code(), far.read_stack()),
            None,
            "a misaligned pc"
        );
    }

    #[test]
    fn epilogue_after_x29_and_x30_are_restored()
    {
        let mut machine = rustc_frame();
        machine.stack.insert(0xff0, 0x2000);
        machine.stack.insert(0xff8, 0x5555);
        // On the `ldp`: everything still to be loaded.
        let state = machine.epilogue(4, &registers(0xfe0, 0xff0, 0)).expect("ldp");
        assert_eq!((state.sp, state.x[29], state.x[30]), (0x1000, 0x2000, 0x5555));
        // On the `add sp`: x29 and x30 are the caller's already.
        let state = machine.epilogue(5, &registers(0xfe0, 0x2000, 0x5555)).expect("add");
        assert_eq!((state.sp, state.x[29], state.x[30]), (0x1000, 0x2000, 0x5555));
        // On the `ret`.
        let state = machine.epilogue(6, &registers(0x1000, 0x2000, 0x5555)).expect("ret");
        assert_eq!(state.sp, 0x1000);
        // In the body, a call comes first.
        let body = Machine::new(&[MOV_X8_X0, BL, LDP_FP_LR_SP_0X10, ADD_SP_0X20, RET]);
        assert_eq!(body.epilogue(0, &registers(0xfe0, 0xff0, 0)), None);
    }

    #[test]
    fn epilogue_of_a_frameless_function_after_its_stack_was_released()
    {
        // `sub sp, sp, #0x20; ...; add sp, sp, #0x20; ret`: stopped on the `ret`, the
        // caller's sp is sp itself, not sp + 0x20.
        let machine = Machine::new(&[SUB_SP_0X20, MOV_X8_X0, ADD_SP_0X20, RET]);
        let state = machine.epilogue(3, &registers(0x1000, 0x2000, 0x5555)).expect("ret");
        assert_eq!((state.sp, state.x[30]), (0x1000, 0x5555));
        let state = machine.epilogue(1, &registers(0xfe0, 0x2000, 0x5555)).expect("body");
        assert_eq!(state.sp, 0x1000);
        assert_eq!(state.known & (1 << 8), 0, "x8 was overwritten");
    }

    #[test]
    fn epilogue_restores_callee_registers_and_supports_frame_pointer_resets()
    {
        let mut machine = Machine::new(&[MOV_SP_FP, LDP_FP_LR_POST_0X10, LDP_X20_X19_POST_0X20, AUTIBSP, RETAB]);
        machine.stack.insert(0xfd0, 0x2000);
        machine.stack.insert(0xfd8, 0x5555);
        machine.stack.insert(0xfe0, 0x2020);
        machine.stack.insert(0xfe8, 0x1919_0000);
        let state = machine.epilogue(0, &registers(0xf00, 0xfd0, 0)).expect("epilogue");
        assert_eq!(state.sp, 0x1000);
        assert_eq!(
            (state.x[19], state.x[20], state.x[29], state.x[30]),
            (0x1919_0000, 0x2020, 0x2000, 0x5555)
        );
        // `b` over a word that is not code, then the epilogue; a branch to itself never
        // reaches a `ret`.
        let mut branch = Machine::new(&[0x1400_0002, BRK_0, LDP_FP_LR_POST_0X10, RET]);
        branch.stack.insert(0xff0, 0x2000);
        branch.stack.insert(0xff8, 0x5555);
        let state = branch.epilogue(0, &registers(0xff0, 0xff0, 0)).expect("through the b");
        assert_eq!((state.sp, state.x[29], state.x[30]), (0x1000, 0x2000, 0x5555));
        assert_eq!(Machine::new(&[B]).epilogue(0, &registers(0xff0, 0xff0, 0)), None);
        // A value stored by the simulated code is read back, not the stale stack word.
        let mut spill = Machine::new(&[STR_X19_SP_8, LDR_X19_SP_8, RET]);
        spill.stack.insert(0x1008, 0xdead);
        let state = spill.epilogue(0, &registers(0x1000, 0, 0x5555)).expect("spill");
        assert_eq!(state.x[19], 0x1919);
        let unreadable = Machine::new(&[LDP_FP_LR_POST_0X10, RET]);
        assert_eq!(unreadable.epilogue(0, &registers(0xfd0, 0xfd0, 0)), None);
    }

    /// A single-register store touches only its own slot: the word above it is not marked
    /// as stored, so the frame record loaded from there keeps its value.
    #[test]
    fn epilogue_single_store_below_the_frame_record_leaves_the_record_readable()
    {
        // `str x0, [sp, #8]; ldp x29, x30, [sp, #0x10]; add sp, sp, #0x20; ret`.
        const STR_X0_SP_8: u32 = 0xF900_07E0;
        let mut machine = Machine::new(&[STR_X0_SP_8, LDP_FP_LR_SP_0X10, ADD_SP_0X20, RET]);
        machine.stack.insert(0xff0, 0x2000);
        machine.stack.insert(0xff8, 0x5555);
        let state = machine.epilogue(0, &registers(0xfe0, 0xff0, 0)).expect("epilogue");
        assert_ne!(state.known & (1 << 29), 0, "x29 is known");
        assert_eq!((state.sp, state.x[29], state.x[30]), (0x1000, 0x2000, 0x5555));
    }

    /// A single-register store in the top slot of the allocation is understood.
    #[test]
    fn prologue_single_store_in_the_top_slot()
    {
        // `sub sp, sp, #0x10; str x30, [sp, #8]`, stopped after the store.
        const SUB_SP_0X10: u32 = 0xD100_43FF;
        const STR_X30_SP_8: u32 = 0xF900_07FE;
        let mut machine = Machine::new(&[SUB_SP_0X10, STR_X30_SP_8, NOP]);
        machine.stack.insert(0xff8, 0x5555);
        let state = caller(machine.prologue(2, &registers(0xff0, 0x2000, 0x7777)));
        assert_eq!((state.sp, state.x[30]), (0x1000, 0x5555));
    }
}
