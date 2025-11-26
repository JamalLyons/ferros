//! # Stack Unwinding
//!
//! DWARF CFI (Call Frame Information) based stack unwinding with fallback heuristics.
//!
//! This module implements stack unwinding using DWARF CFI from `.eh_frame` and `.debug_frame`
//! sections, with fallback strategies for when CFI is unavailable:
//!
//! 1. **CFI-based unwinding**: Uses DWARF CFI to compute CFA (Canonical Frame Address) and
//!    register recovery rules.
//! 2. **Frame pointer fallback**: Uses the frame pointer register (RBP on x86-64, X29 on ARM64)
//!    to walk the stack.
//! 3. **Stack scan fallback**: Scans the stack for values that look like return addresses.
//! 4. **Link register fallback**: Uses the link register (LR on ARM64) as the return address.
//!
//! ## DWARF CFI Sections
//!
//! - **`.eh_frame`**: Exception handling frame information (used at runtime)
//! - **`.debug_frame`**: Debug frame information (used by debuggers)
//!
//! Both sections contain FDEs (Frame Description Entries) that describe how to unwind
//! the stack for each function.
//!
//! ## References
//!
//! - [DWARF Debugging Information Format](https://dwarfstd.org/)
//! - [DWARF CFI Specification](https://dwarfstd.org/doc/DWARF5.pdf#page=179)
//! - [gimli crate documentation](https://docs.rs/gimli/latest/gimli/)

use gimli::{
    self, BaseAddresses, CfaRule, DebugFrame, EhFrame, EhFrameHdr, Register, RegisterRule, UnwindContext, UnwindSection,
};

use crate::error::{DebuggerError, Result};
use crate::symbols::{BinaryImage, SymbolCache, SymbolFrame, Symbolication};
use crate::types::{Address, Architecture, FrameId, FrameKind, FrameStatus, Registers, StackFrame, ThreadId};

/// Minimal memory accessor required for stack unwinding.
///
/// This trait allows the unwinder to read memory from the target process
/// to recover register values and compute the CFA (Canonical Frame Address).
///
/// ## Implementation Notes
///
/// Implementations should handle:
/// - Invalid memory addresses (return errors, don't panic)
/// - Partial reads (if memory is partially unmapped)
/// - Endianness (matches target architecture)
pub trait MemoryAccess
{
    /// Read a 64-bit value from the given address.
    ///
    /// Returns an error if the address is invalid or unreadable.
    fn read_u64(&self, address: Address) -> Result<u64>;
}

/// CFI-driven stack unwinder with frame-pointer fallbacks.
///
/// This unwinder attempts to build a call stack by:
///
/// 1. Using DWARF CFI from `.eh_frame` or `.debug_frame` sections
/// 2. Falling back to frame pointer walking if CFI is unavailable
/// 3. Falling back to stack scanning if frame pointer is unavailable
/// 4. Falling back to link register (ARM64) if available
///
/// ## Architecture Support
///
/// - **x86-64**: Uses RBP (frame pointer) and RSP (stack pointer)
/// - **ARM64**: Uses X29 (frame pointer), X30 (link register), and SP (stack pointer)
///
/// ## Inlined Frames
///
/// The unwinder detects inlined functions using DWARF line information and creates
/// multiple `StackFrame` entries for a single physical frame when inlining is present.
pub struct StackUnwinder<'a, M>
{
    architecture: Architecture,
    symbols: &'a SymbolCache,
    memory: &'a M,
}

impl<'a, M: MemoryAccess> StackUnwinder<'a, M>
{
    /// Create a new stack unwinder.
    ///
    /// ## Parameters
    ///
    /// - `architecture`: Target architecture (x86-64 or ARM64)
    /// - `symbols`: Symbol cache for resolving addresses to functions
    /// - `memory`: Memory accessor for reading process memory
    pub fn new(architecture: Architecture, symbols: &'a SymbolCache, memory: &'a M) -> Self
    {
        Self {
            architecture,
            symbols,
            memory,
        }
    }

    /// Unwind the call stack starting from the given registers.
    ///
    /// This method walks the stack frame by frame, using CFI when available
    /// and falling back to heuristics when CFI is unavailable.
    ///
    /// ## Parameters
    ///
    /// - `thread`: Thread ID for frame identification
    /// - `regs`: Initial register state (PC, SP, FP, etc.)
    /// - `max_frames`: Maximum number of frames to unwind
    ///
    /// ## Returns
    ///
    /// A vector of `StackFrame` entries, ordered from most recent (top) to oldest (bottom).
    /// Each frame includes:
    /// - Frame ID (stable identifier)
    /// - Program counter (PC)
    /// - Stack pointer (SP)
    /// - Frame pointer (FP) if available
    /// - Symbolication (function name, source location)
    /// - Frame status (complete, incomplete, error)
    ///
    /// ## Frame Status
    ///
    /// - `Complete`: Frame was successfully unwound with CFI
    /// - `Incomplete`: Frame was unwound using fallback heuristics
    /// - `Error`: Frame unwinding failed
    ///
    /// ## Errors
    ///
    /// Returns an error if memory reads fail or CFI parsing fails.
    pub fn unwind(&self, thread: ThreadId, regs: &Registers, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        let mut frames = Vec::new();
        let mut cursor = regs.clone();
        let mut depth: u32 = 0;
        let mut status = FrameStatus::Complete;
        let mut return_address = None;

        while depth < max_frames as u32 && cursor.pc != Address::ZERO {
            let symbolication = self.symbols.symbolicate(cursor.pc);
            // Only log if we have an image for this address but still can't symbolicate it
            // (this indicates a real problem, not just a missing system library)
            if symbolication.is_none() {
                if self.symbols.image_for_address(cursor.pc).is_some() {
                    use tracing::debug;
                    debug!(
                        "No symbolication for address 0x{:x} (image loaded but symbolication failed)",
                        cursor.pc.value()
                    );
                }
                // Otherwise, it's expected - address is in a system library we haven't loaded
            }
            append_logical_frames(&mut frames, thread, depth, &cursor, &symbolication, status, return_address);

            if frames.len() >= max_frames {
                break;
            }

            let outcome = self
                .unwind_once(&cursor)?
                .or_else(|| self.frame_pointer_fallback(&cursor).transpose().ok().flatten())
                .or_else(|| self.stack_scan_fallback(&cursor).transpose().ok().flatten())
                .or_else(|| self.link_register_fallback(&cursor).transpose().ok().flatten());

            let Some(outcome) = outcome else {
                break;
            };

            cursor = outcome.next;
            return_address = outcome.return_address;
            status = outcome.status;
            depth += 1;

            if frames.len() >= max_frames {
                break;
            }
        }

        Ok(frames)
    }

    /// Attempt a single unwind step using available DWARF metadata for the image that
    /// contains the supplied program counter.
    fn unwind_once(&self, regs: &Registers) -> Result<Option<UnwindStep>>
    {
        let Some(image) = self.symbols.image_for_address(regs.pc) else {
            return Ok(None);
        };
        if let Some(step) = self.try_unwind_eh_frame(&image, regs)? {
            return Ok(Some(step));
        }
        if let Some(step) = self.try_unwind_debug_frame(&image, regs)? {
            return Ok(Some(step));
        }
        Ok(None)
    }

    /// Evaluate a DWARF register rule using the provided register state and computed
    /// Canonical Frame Address.
    fn evaluate_rule(&self, rule: &RegisterRule<usize>, regs: &Registers, cfa: u64) -> Result<u64>
    {
        match rule {
            RegisterRule::Undefined | RegisterRule::SameValue => {
                Err(DebuggerError::InvalidArgument("register rule unavailable".into()))
            }
            RegisterRule::Offset(offset) => {
                let addr = Address::from((cfa as i64 + *offset) as u64);
                self.memory.read_u64(addr)
            }
            RegisterRule::ValOffset(offset) => Ok((cfa as i64 + *offset) as u64),
            RegisterRule::Register(register) => read_register_value(self.architecture, regs, *register)
                .ok_or_else(|| DebuggerError::InvalidArgument("register redirect missing value".into())),
            _ => Err(DebuggerError::InvalidArgument(
                "unsupported CFI expression encountered".into(),
            )),
        }
    }

    /// Attempt to unwind the current frame using `.eh_frame` data (and header table
    /// when available) from the binary image that owns the PC.
    fn try_unwind_eh_frame(&self, image: &BinaryImage, regs: &Registers) -> Result<Option<UnwindStep>>
    {
        let Some((eh_vmaddr, eh_bytes)) = image.eh_frame_section() else {
            return Ok(None);
        };

        let (text_base, _) = image.runtime_range();
        let mut bases = BaseAddresses::default()
            .set_text(text_base)
            .set_eh_frame(image.relocated_address(eh_vmaddr));
        if let Some((hdr_vmaddr, _)) = image.eh_frame_hdr_section() {
            bases = bases.set_eh_frame_hdr(image.relocated_address(hdr_vmaddr));
        }

        let mut eh_frame = EhFrame::new(eh_bytes, image.endian());
        eh_frame.set_address_size(self.architecture.pointer_size_bytes());

        if let Some((_hdr_vmaddr, hdr_bytes)) = image.eh_frame_hdr_section() {
            let header = EhFrameHdr::new(hdr_bytes, image.endian());
            if let Some(step) = self.unwind_with_eh_frame_hdr(&eh_frame, header, bases.clone(), regs)? {
                return Ok(Some(step));
            }
        }

        self.unwind_with_cfi(&eh_frame, bases, regs)
    }

    /// Attempt to unwind the current frame using `.debug_frame` data when the runtime
    /// `.eh_frame` path is unavailable.
    fn try_unwind_debug_frame(&self, image: &BinaryImage, regs: &Registers) -> Result<Option<UnwindStep>>
    {
        let Some((df_vmaddr, df_bytes)) = image.debug_frame_section() else {
            return Ok(None);
        };

        let (text_base, _) = image.runtime_range();
        // In gimli 0.32, debug_frame uses the same base address mechanism
        let bases = BaseAddresses::default()
            .set_text(text_base)
            .set_eh_frame(image.relocated_address(df_vmaddr));

        let mut debug_frame = DebugFrame::new(df_bytes, image.endian());
        debug_frame.set_address_size(self.architecture.pointer_size_bytes());
        self.unwind_with_cfi(&debug_frame, bases, regs)
    }

    /// Translate a resolved DWARF unwind row into the `Registers` describing the next
    /// frame. Returns `Ok(None)` when the row cannot produce a valid next PC.
    fn build_step_from_row(&self, regs: &Registers, row: &gimli::UnwindTableRow<usize>) -> Result<Option<UnwindStep>>
    {
        let cfa = match row.cfa() {
            CfaRule::RegisterAndOffset { register, offset } => {
                let base = read_register_value(self.architecture, regs, *register)
                    .ok_or_else(|| DebuggerError::InvalidArgument("missing register for CFA evaluation".to_string()))?;
                (base as i64 + offset) as u64
            }
            _ => return Ok(None),
        };

        let return_reg = return_register(self.architecture);
        let rule = row.register(return_reg);
        let (pc, status) = match rule {
            RegisterRule::Undefined | RegisterRule::SameValue => (Address::ZERO, FrameStatus::CfiFallback),
            _ => match self.evaluate_rule(&rule, regs, cfa) {
                Ok(value) => (Address::from(value), FrameStatus::Complete),
                Err(_) => (Address::ZERO, FrameStatus::CfiFallback),
            },
        };

        if pc == Address::ZERO {
            return Ok(None);
        }

        let mut next = regs.clone();
        next.sp = Address::from(cfa);
        next.pc = pc;

        Ok(Some(UnwindStep {
            next,
            return_address: Some(pc),
            status,
        }))
    }

    /// Walk the frame-pointer chain (RBP/X29) when structured unwind info is missing.
    fn frame_pointer_fallback(&self, regs: &Registers) -> Option<Result<UnwindStep>>
    {
        match self.architecture {
            Architecture::Arm64 => {
                let fp = regs.fp;
                if fp == Address::ZERO {
                    return None;
                }

                let saved_fp = match self.memory.read_u64(fp) {
                    Ok(value) => value,
                    Err(err) => return Some(Err(err)),
                };
                let saved_lr = match self.memory.read_u64(Address::from(fp.value() + 8)) {
                    Ok(value) => value,
                    Err(err) => return Some(Err(err)),
                };

                let mut next = regs.clone();
                next.fp = Address::from(saved_fp);
                next.sp = Address::from(fp.value() + 16);
                next.pc = Address::from(saved_lr);

                Some(Ok(UnwindStep {
                    next,
                    return_address: Some(Address::from(saved_lr)),
                    status: FrameStatus::CfiFallback,
                }))
            }
            Architecture::X86_64 => {
                let fp = regs.fp;
                if fp == Address::ZERO {
                    return None;
                }

                let saved_fp = match self.memory.read_u64(fp) {
                    Ok(value) => value,
                    Err(err) => return Some(Err(err)),
                };
                let return_addr = match self.memory.read_u64(Address::from(fp.value() + 8)) {
                    Ok(value) => value,
                    Err(err) => return Some(Err(err)),
                };

                let mut next = regs.clone();
                next.fp = Address::from(saved_fp);
                next.sp = Address::from(fp.value() + 16);
                next.pc = Address::from(return_addr);

                Some(Ok(UnwindStep {
                    next,
                    return_address: Some(Address::from(return_addr)),
                    status: FrameStatus::CfiFallback,
                }))
            }
            _ => None,
        }
    }

    /// Heuristically scan the stack for a plausible return address when both CFI and
    /// frame-pointer strategies fail.
    fn stack_scan_fallback(&self, regs: &Registers) -> Option<Result<UnwindStep>>
    {
        if regs.sp == Address::ZERO {
            return None;
        }

        match self.architecture {
            Architecture::Arm64 | Architecture::X86_64 => {
                let return_addr = match self.memory.read_u64(regs.sp) {
                    Ok(value) if value != 0 && value != regs.pc.value() => value,
                    Ok(_) => return None,
                    Err(err) => return Some(Err(err)),
                };

                let mut next = regs.clone();
                next.sp = Address::from(regs.sp.value() + 8);
                next.pc = Address::from(return_addr);

                Some(Ok(UnwindStep {
                    next,
                    return_address: Some(Address::from(return_addr)),
                    status: FrameStatus::Heuristic,
                }))
            }
            _ => None,
        }
    }

    /// ARM64-only heuristic that treats the link register as the next return address.
    fn link_register_fallback(&self, regs: &Registers) -> Option<Result<UnwindStep>>
    {
        if self.architecture != Architecture::Arm64 {
            return None;
        }

        let lr = regs.general.get(30).copied()?;
        if lr == 0 || lr == regs.pc.value() {
            return None;
        }

        let mut next = regs.clone();
        next.pc = Address::from(lr);

        Some(Ok(UnwindStep {
            next,
            return_address: Some(Address::from(lr)),
            status: FrameStatus::Heuristic,
        }))
    }
}

impl<'a, M: MemoryAccess> StackUnwinder<'a, M>
{
    /// Use the `.eh_frame_hdr` index to locate an FDE quickly and evaluate the unwind
    /// row for the current PC.
    fn unwind_with_eh_frame_hdr<R>(
        &self,
        eh_frame: &EhFrame<R>,
        header: EhFrameHdr<R>,
        bases: BaseAddresses,
        regs: &Registers,
    ) -> Result<Option<UnwindStep>>
    where
        R: gimli::Reader<Offset = usize>,
    {
        let parsed = header
            .parse(&bases, self.architecture.pointer_size_bytes())
            .map_err(|err| map_gimli_error("parsing .eh_frame_hdr", err))?;
        let Some(table) = parsed.table() else {
            return Ok(None);
        };

        let pc = regs.pc.value();
        let pointer = table
            .lookup(pc, &bases)
            .map_err(|err| map_gimli_error("looking up FDE in .eh_frame_hdr", err))?;
        let offset = table
            .pointer_to_offset(pointer)
            .map_err(|err| map_gimli_error("resolving FDE pointer", err))?;

        let partial = eh_frame
            .partial_fde_from_offset(&bases, offset)
            .map_err(|err| map_gimli_error("loading FDE from .eh_frame_hdr", err))?;
        let fde = partial
            .parse(|section, base_addresses, cie_offset| section.cie_from_offset(base_addresses, cie_offset))
            .map_err(|err| map_gimli_error("parsing frame description entry", err))?;

        if !fde.contains(pc) {
            return Ok(None);
        }

        let mut ctx = UnwindContext::<usize>::new();
        match fde.unwind_info_for_address(eh_frame, &bases, &mut ctx, pc) {
            Ok(row) => self.build_step_from_row(regs, row),
            Err(gimli::Error::NoUnwindInfoForAddress) => Ok(None),
            Err(err) => Err(map_gimli_error("evaluating unwind row", err)),
        }
    }

    /// Iterate through an unwind section, parsing each FDE until one contains the
    /// current PC, then evaluate it to produce the next frame.
    fn unwind_with_cfi<R, Section>(
        &self,
        section: &Section,
        bases: BaseAddresses,
        regs: &Registers,
    ) -> Result<Option<UnwindStep>>
    where
        R: gimli::Reader<Offset = usize>,
        Section: gimli::UnwindSection<R>,
    {
        let pc = regs.pc.value();
        let mut entries = section.entries(&bases);
        let mut ctx = UnwindContext::<usize>::new();
        while let Some(entry) = entries.next().map_err(|err| map_gimli_error("reading unwind entry", err))? {
            let gimli::CieOrFde::Fde(partial) = entry else {
                continue;
            };
            let fde = partial
                .parse(|unwind_section, base_addresses, cie_offset| {
                    unwind_section.cie_from_offset(base_addresses, cie_offset)
                })
                .map_err(|err| map_gimli_error("parsing frame description entry", err))?;
            if !fde.contains(pc) {
                continue;
            }

            match fde.unwind_info_for_address(section, &bases, &mut ctx, pc) {
                Ok(row) => {
                    if let Some(step) = self.build_step_from_row(regs, row)? {
                        return Ok(Some(step));
                    }
                }
                Err(gimli::Error::NoUnwindInfoForAddress) => continue,
                Err(err) => return Err(map_gimli_error("evaluating unwind row", err)),
            }
        }

        Ok(None)
    }
}

struct UnwindStep
{
    next: Registers,
    return_address: Option<Address>,
    status: FrameStatus,
}

fn append_logical_frames(
    frames: &mut Vec<StackFrame>,
    thread: ThreadId,
    depth: u32,
    regs: &Registers,
    symbolication: &Option<Symbolication>,
    status: FrameStatus,
    return_address: Option<Address>,
)
{
    if let Some(symbols) = symbolication {
        for (inline_depth, SymbolFrame { symbol, location }) in symbols.frames.iter().enumerate() {
            let inline_id = FrameId::new(thread, depth, inline_depth as u8, regs.pc, regs.sp);
            frames.push(StackFrame {
                id: inline_id,
                thread,
                index: frames.len(),
                kind: FrameKind::Inlined {
                    physical: inline_id,
                    depth: inline_depth as u8,
                },
                pc: regs.pc,
                sp: regs.sp,
                fp: regs.fp,
                return_address,
                symbol: Some(symbol.clone()),
                location: location.clone(),
                status,
            });
        }
    }

    let physical_id = FrameId::new(thread, depth, 0, regs.pc, regs.sp);
    let (symbol, location) = symbolication
        .as_ref()
        .and_then(|sym| sym.frames.first())
        .map(|frame| (Some(frame.symbol.clone()), frame.location.clone()))
        .unwrap_or((None, None));

    frames.push(StackFrame {
        id: physical_id,
        thread,
        index: frames.len(),
        kind: FrameKind::Physical,
        pc: regs.pc,
        sp: regs.sp,
        fp: regs.fp,
        return_address,
        symbol,
        location,
        status,
    });
}
fn read_register_value(architecture: Architecture, regs: &Registers, register: Register) -> Option<u64>
{
    let reg_num = register.0;
    match architecture {
        Architecture::Arm64 => {
            // ARM64 DWARF register mapping
            match reg_num {
                31 => Some(regs.sp.value()),         // SP
                30 => regs.general.get(30).copied(), // LR
                29 => Some(regs.fp.value()),         // FP
                0..=28 => regs.general.get(reg_num as usize).copied(),
                _ => None,
            }
        }
        Architecture::X86_64 => {
            // x86-64 DWARF register mapping
            match reg_num {
                7 => Some(regs.sp.value()),  // RSP
                16 => Some(regs.pc.value()), // RIP
                6 => Some(regs.fp.value()),  // RBP
                0..=15 => {
                    // Map DWARF register numbers to our general register array
                    let mapping = [0, 2, 1, 3, 6, 7, 5, 4, 8, 9, 10, 11, 12, 13, 14, 15];
                    mapping.get(reg_num as usize).and_then(|&idx| regs.general.get(idx).copied())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn return_register(architecture: Architecture) -> Register
{
    match architecture {
        Architecture::Arm64 => Register(30),  // LR
        Architecture::X86_64 => Register(16), // RA (return address, same as RIP)
        _ => Register(0),
    }
}

fn map_gimli_error(context: &str, err: gimli::Error) -> DebuggerError
{
    DebuggerError::InvalidArgument(format!("{context}: {err}"))
}
