use gimli::{self, BaseAddresses, CfaRule, DebugFrame, Register, RegisterRule};

use crate::error::{DebuggerError, Result};
use crate::symbols::{BinaryImage, SymbolCache, SymbolFrame, Symbolication};
use crate::types::{Address, Architecture, FrameId, FrameKind, FrameStatus, Registers, StackFrame, ThreadId};

/// Minimal memory accessor required for stack unwinding.
pub trait MemoryAccess
{
    fn read_u64(&self, address: Address) -> Result<u64>;
}

/// CFI-driven stack unwinder with frame-pointer fallbacks.
pub struct StackUnwinder<'a, M>
{
    architecture: Architecture,
    symbols: &'a SymbolCache,
    memory: &'a M,
}

impl<'a, M: MemoryAccess> StackUnwinder<'a, M>
{
    pub fn new(architecture: Architecture, symbols: &'a SymbolCache, memory: &'a M) -> Self
    {
        Self {
            architecture,
            symbols,
            memory,
        }
    }

    pub fn unwind(&self, thread: ThreadId, regs: &Registers, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        let mut frames = Vec::new();
        let mut cursor = regs.clone();
        let mut depth: u32 = 0;
        let mut status = FrameStatus::Complete;
        let mut return_address = None;

        while depth < max_frames as u32 && cursor.pc != Address::ZERO {
            let symbolication = self.symbols.symbolicate(cursor.pc);
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

        // TODO: Implement proper FDE lookup using gimli 0.32 API
        // For now, return None to allow fallback heuristics
        // The gimli 0.32 API requires iterating FDEs and checking address ranges
        let _eh_frame = gimli::EhFrame::new(eh_bytes, image.endian());
        let _bases = bases;
        let _pc = regs.pc.value();

        Ok(None)
    }

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

        // TODO: Implement proper FDE lookup for .debug_frame using gimli 0.32 API
        let _debug_frame = DebugFrame::new(df_bytes, image.endian());
        let _bases = bases;
        let _pc = regs.pc.value();

        Ok(None)
    }

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

#[allow(dead_code)]
fn return_register(architecture: Architecture) -> Register
{
    match architecture {
        Architecture::Arm64 => Register(30),  // LR
        Architecture::X86_64 => Register(16), // RA (return address, same as RIP)
        _ => Register(0),
    }
}
