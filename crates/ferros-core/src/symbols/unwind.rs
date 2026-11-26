//! # Stack Unwinding
//!
//! Walks a stopped thread's stack from its registers, innermost frame first.
//!
//! ## Unwind sources and their order
//!
//! An image's [`ObjectFormat`](crate::objfile::ObjectFormat) can supply a per-function
//! unwind table ([`UnwindHints`](crate::objfile::UnwindHints)) besides the image's call
//! frame information. On an arm64 Rust binary the platform's table has an entry for every
//! function, and most say "use the FDE at this `eh_frame` offset"; leaves without an FDE
//! (such as `__rust_alloc`) and C shims with a plain frame record (such as the C `main`
//! shim) have hints of their own and no FDE. Those FDE rows describe the prologue exactly,
//! including the first instruction (`CFA=sp`, return address still in `x30`). Frameless
//! leaves do not set up `x29`: there `fp` is still the caller's, so following the
//! frame-pointer chain from such a leaf skips its caller.
//!
//! Each step looks up the hint of the function containing the lookup address (the pc of
//! the innermost frame, the call instruction of a caller) and uses:
//!
//! 1. **[`UnwindHint::Cfi`]**: the FDE at the hinted offset in `eh_frame`, evaluated
//!    directly (no search: the main `ferros` binary has about 34,000 FDEs). If it cannot
//!    be evaluated, the frame-pointer chain.
//! 2. **[`UnwindHint::Leaf`]**: in the innermost frame, the caller is the link register
//!    and its `sp` is `sp + stack_size` (`sp` itself on the first instruction, before the
//!    allocation). Elsewhere the instructions are analysed first (see *Prologues and
//!    epilogues* below), so a stop after the epilogue released the allocation gets the
//!    caller's `sp` right. A frameless leaf makes no calls, so it cannot be a caller frame;
//!    there the walk stops instead of guessing.
//! 3. **[`UnwindHint::FrameRecord`]**: the frame-pointer chain: the record at `fp` holds
//!    the caller's `fp` and the return address. On the function's first instruction
//!    (innermost frame only) the record is not built yet, so the caller is the link
//!    register. A table may give one entry to a run of adjacent functions with the same
//!    hint, so a first instruction is recognized from the entry or from the DWARF function
//!    index. Elsewhere in the innermost frame the instructions are analysed first (see
//!    *Prologues and epilogues*), which covers the rest of the prologue, the epilogue after
//!    `x29`/`x30` are reloaded, and a shrink-wrapped path before the record is built.
//! 4. **No hint** (no table, no entry, or an entry without information): `.eh_frame_hdr`
//!    lookup, else a linear scan of `eh_frame` (FDE ranges as runtime addresses), then of
//!    `.debug_frame` (FDE ranges as file addresses, since it may live in an unslid
//!    companion file); then, in the innermost frame on the first instruction of a
//!    function known to DWARF, the link register; then the frame-pointer chain.
//!
//! ## Prologues and epilogues
//!
//! A hint cannot describe the instructions of a frame-record function before `x29` is set
//! to its frame record or after the epilogue reloads `x29`/`x30`, nor a leaf after its
//! epilogue released the stack. For an innermost frame with those hints, past the first
//! instruction, the unwinder decodes the instructions around the pc, read with
//! [`MemoryAccess::read_u32`]:
//!
//! 1. **Epilogue**: from the pc forward, the straight-line code to the `ret` (`ret`,
//!    `retaa`, `retab`) is executed on the instructions that move `sp` or reload
//!    registers from the stack. The registers at the `ret` are the caller's: the return
//!    address in `x30`, `sp`, `x29` and the reloaded `x19`-`x28`.
//! 2. **Prologue**: otherwise, from the function's start up to the pc. The start is the
//!    later of the hint's start and the nearest DWARF function start (a table may merge
//!    entries). If an instruction sets `x29` from `sp`, the record is built
//!    and the frame-pointer chain applies. Otherwise the caller's `sp` is `sp` plus what
//!    the code allocated, and `x29`, `x30`, `x19`-`x28` come from where it stored them,
//!    else they still hold the caller's values.
//! 3. If neither is understood (a branch or call on the way, an instruction that is not
//!    decoded, an unreadable word), the hint's rule above applies unchanged.
//!
//! Recognized instructions include `stp`/`ldp`/`str`/`ldr` through `sp` (offset,
//! pre-index, post-index), `sub`/`add sp, sp, #N`, `add x29, sp, #N` and `mov x29, sp`,
//! `mov sp, x29`, and hints such as `pacibsp`/`autibsp`; ordinary data processing is
//! neutral. The session's [`MemoryAccess`] hides enabled software breakpoints behind
//! their original bytes, so a stop at a breakpoint inside a prologue or epilogue still
//! decodes the real instructions.
//!
//! Images whose files could not be loaded (for example system libraries the loader maps
//! from a shared region rather than from a file) have none of this, and use the
//! frame-pointer chain.
//!
//! Before any of these, a caller frame is checked for a signal trampoline (see
//! [Signal handler frames](#signal-handler-frames)).
//!
//! The link register only holds a return address for the innermost frame (its value is
//! known only there); a caller frame never uses it. There is no stack-scan heuristic: a
//! frame is either derived from unwind information or not produced.
//!
//! ## Registers in caller frames
//!
//! A caller frame keeps the known callee-saved registers selected by its
//! [`Arch`] plus registers its CFI restores. Unknown general registers read as zero.
//! Rules that use an unknown register fail.
//!
//! ## Signal handler frames
//!
//! A kernel delivers a signal by saving the interrupted register state on the thread's
//! stack (or on an alternate signal stack) and starting the thread in a trampoline that
//! calls the handler. The trampoline's own frame record usually holds the interrupted
//! `fp` and link register, so following it gives the interrupted function's caller (or a
//! stale link register), and the interrupted function is skipped.
//!
//! How a trampoline frame is recognized and where the saved state lies belong to the
//! platform: the unwinder asks the session's
//! [`TargetRuntime::signal_context`]
//! about every caller frame (arm64 only) before any other rule. A
//! [`StackUnwinder::new`] without [`with_runtime`](StackUnwinder::with_runtime) finds
//! none. When the runtime returns a [`SignalContext`], the trampoline's caller is the
//! interrupted frame, with every register restored from it (all known, `x30` included),
//! and it is treated like an innermost frame: its pc is the interrupted instruction, not
//! a return address, so it is looked up at `pc` rather than `pc - 4`, and the
//! innermost-frame rules (link register, first instruction, frameless leaf) apply to it.
//! Its status is [`FrameStatus::Complete`]: the whole register state is known.
//!
//! The interrupted `sp` must lie at or above the end of the saved state (the signal was
//! taken on the thread's own stack) or below the trampoline's `sp` (an alternate signal
//! stack, possibly at lower addresses); anywhere in between it would be among the
//! handler's own frames and the context is rejected. The alternate-stack step alone is
//! exempt from the rule that `sp` never moves down (see [Termination](#termination)):
//! the interrupted `sp` must instead be non-zero and 16-byte aligned, and the
//! interrupted pc executable. Every later step is checked against the interrupted `sp`
//! again.
//!
//! ## Termination
//!
//! The walk stops cleanly, without an error, when the return address is zero, when no
//! source applies, when the frame-pointer chain ends (`fp == 0`) or is not strictly
//! increasing, when the stack pointer would move down (except into the frame a signal
//! on an alternate signal stack interrupted), when memory cannot be read, or when
//! the next pc is not in executable memory ([`MemoryAccess::is_executable`]). This is how
//! the thread base (the thread entry point, or the loader's entry point on the main
//! thread) ends a trace.
//!
//! ## Pointer authentication
//!
//! Every recovered return address (from memory or from the link register) goes through
//! [`Arch::strip_pointer_auth`]. How many address bits it keeps is the target's choice,
//! made by the [`Arch`] the unwinder is given (the session passes the one its backend
//! configured).
//!
//! ## Caller frames
//!
//! A caller frame's pc is the return address, the instruction after the call. Its function
//! and source line are looked up at the call instruction (`pc - 4` on arm64), and its
//! unwind row too, so the reported line is the call site and a call that is the last
//! instruction of a function still finds that function.
//!
//! ## Map of this file
//!
//! - `MemoryAccess`: the memory the walk reads.
//! - `StackUnwinder`: `new`, `with_runtime`, `unwind`; the per-step logic is in its `impl`
//!   (`unwind_with_lookups` and the CFI, leaf and frame-record steps described above).
//! - `hinted`: which [`UnwindHint`] applies to a lookup address.
//! - `append_logical_frames`: inlined frames for one physical frame.
//! - Register helpers (`general_bit`, `read_register_value`, `return_register`); tests in
//!   `mod tests`.
//!
//! ## References
//!
//! - [DWARF 5, section 6.4 Call Frame Information](https://dwarfstd.org/doc/DWARF5.pdf)
//! - [gimli crate documentation](https://docs.rs/gimli/latest/gimli/)

use gimli::{
    BaseAddresses, CfaRule, DebugFrame, EhFrame, EhFrameHdr, EhFrameOffset, EndianSlice, Register, RegisterRule,
    RunTimeEndian, UnwindContext, UnwindSection,
};

use crate::arch::arm64_frame::{self, FrameRegisters, Prologue};
use crate::arch::{Arch, Arm64, UndefinedReturnAddress};
use crate::error::Result;
use crate::objfile::UnwindHint;
use crate::runtime::{NoRuntime, SignalContext, TargetRuntime, TrampolineFrame};
use crate::symbols::{BinaryImage, SymbolCache, SymbolFrame, Symbolication};
use crate::types::{Address, Architecture, FrameId, FrameKind, FrameStatus, Registers, StackFrame, ThreadId};

/// Minimal memory accessor required for stack unwinding.
///
/// Implementations return an error for unreadable addresses rather than panicking.
pub trait MemoryAccess
{
    /// Read a 64-bit little-endian value from the given address.
    ///
    /// # Errors
    ///
    /// An error if the address is invalid or unreadable.
    fn read_u64(&self, address: Address) -> Result<u64>;

    /// Read a 32-bit little-endian value (an arm64 instruction) from a 4-byte aligned
    /// address. The default reads the aligned 64-bit word containing it, which never
    /// crosses a page.
    ///
    /// # Errors
    ///
    /// An error if the address is invalid or unreadable.
    fn read_u32(&self, address: Address) -> Result<u32>
    {
        let value = address.value();
        let word = self.read_u64(Address::new(value & !7))?;
        let shift = (value & 4) * 8;
        Ok(u32::try_from((word >> shift) & 0xFFFF_FFFF).unwrap_or_default())
    }

    /// Whether `address` lies in executable memory. The unwinder stops before a frame
    /// whose pc is not. The default accepts every address.
    fn is_executable(&self, _address: Address) -> bool
    {
        true
    }
}

/// Stack unwinder over CFI and the frame-pointer chain (see the module documentation).
pub struct StackUnwinder<'a, M>
{
    arch: &'a dyn Arch,
    symbols: &'a SymbolCache,
    memory: &'a M,
    runtime: &'a dyn TargetRuntime,
}

/// The runtime of an unwinder that was given none.
static NO_RUNTIME: NoRuntime = NoRuntime;

/// Register state of one physical frame while walking.
#[derive(Clone)]
struct Cursor
{
    regs: Registers,
    /// Bit `n` set means general register slot `n` holds this frame's value. All are
    /// known in the innermost frame. For callers, the unwind step determines which
    /// values are known using the target ABI and any register recovery rules.
    known: u32,
    /// Whether this frame's pc is a return address (every frame but the innermost).
    is_caller: bool,
}

/// What [`StackUnwinder::code_step`] found.
enum CodeStep
{
    /// The caller, from the epilogue or the prologue.
    Caller(UnwindStep),
    /// The prologue built the frame record: the frame-pointer chain applies.
    FrameBuilt,
}

const ALL_GENERAL: u32 = (1 << Arm64::GENERAL_REGISTER_COUNT) - 1;
const FRAME_POINTER: u32 = 1 << Arm64::FRAME_POINTER_INDEX;
const LINK_REGISTER: u32 = 1 << Arm64::LINK_REGISTER_INDEX;

/// One unwind step: the caller's registers and how they were found.
struct UnwindStep
{
    next: Registers,
    known: u32,
    status: FrameStatus,
}

/// What an image's unwind hints say about the function containing an address.
enum Hinted
{
    /// No table, or no entry for the address.
    Absent,
    /// The function's hint, whether the address is its first instruction, and the
    /// runtime address of the entry's first function.
    Hint
    {
        hint: UnwindHint,
        at_entry: bool,
        start: Address,
    },
}

impl<'a, M: MemoryAccess> StackUnwinder<'a, M>
{
    /// Create a stack unwinder.
    ///
    /// - `arch`: target architecture knowledge (registers, pointer authentication)
    /// - `symbols`: loaded images, for CFI and symbolication
    /// - `memory`: reads the target's stack
    ///
    /// It finds no signal handler frames until it is given the platform's runtime
    /// ([`with_runtime`](Self::with_runtime)).
    pub fn new(arch: &'a dyn Arch, symbols: &'a SymbolCache, memory: &'a M) -> Self
    {
        Self {
            arch,
            symbols,
            memory,
            runtime: &NO_RUNTIME,
        }
    }

    /// This unwinder recognizing the signal trampolines of `runtime` (see
    /// [Signal handler frames](self#signal-handler-frames)).
    #[must_use]
    pub fn with_runtime(mut self, runtime: &'a dyn TargetRuntime) -> Self
    {
        self.runtime = runtime;
        self
    }

    /// Unwind at most `max_frames` logical frames (inlined frames included), innermost
    /// first.
    ///
    /// # Errors
    ///
    /// None in practice: every failure to find the next frame ends the trace instead. The
    /// `Result` is kept for implementations of [`MemoryAccess`] whose failures must stop
    /// the walk with an error in the future.
    pub fn unwind(&self, thread: ThreadId, regs: &Registers, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        Ok(self.unwind_with_lookups(thread, regs, max_frames)?.0)
    }

    /// Like [`unwind`](Self::unwind), also returning each frame's lookup address, in the
    /// same order: the address the unwinder symbolicated that frame's physical frame at.
    /// It is the call instruction for a caller frame, and the pc itself for the innermost
    /// frame and for a frame a signal interrupted, which no call reached. Inlined frames
    /// share their physical frame's.
    ///
    /// # Errors
    ///
    /// As [`unwind`](Self::unwind).
    pub(crate) fn unwind_with_lookups(
        &self,
        thread: ThreadId,
        regs: &Registers,
        max_frames: usize,
    ) -> Result<(Vec<StackFrame>, Vec<Address>)>
    {
        let mut frames = Vec::new();
        let mut lookups = Vec::new();
        let mut cursor = Cursor {
            regs: regs.clone(),
            known: ALL_GENERAL,
            is_caller: false,
        };
        let mut status = FrameStatus::Complete;
        let mut depth: u32 = 0;
        // At most one step may move `sp` down: the step out of a signal frame on an
        // alternate signal stack. A second one would let a corrupt context loop back
        // into frames already walked, so it is not taken.
        let mut moved_down = false;

        while frames.len() < max_frames && cursor.regs.pc != Address::ZERO {
            let lookup = self.lookup_address(&cursor);
            let symbolication = self.symbols.symbolicate(lookup);
            let signal = self
                .signal_frame_step(&cursor)
                .filter(|step| !(moved_down && step.next.sp < cursor.regs.sp));
            let (next, interrupted) = match signal {
                Some(step) => {
                    moved_down |= step.next.sp < cursor.regs.sp;
                    (Some(step), true)
                }
                None => (self.step(&cursor), false),
            };
            let return_address = next.as_ref().map(|step| step.next.pc);
            append_logical_frames(
                &mut frames,
                max_frames,
                thread,
                depth,
                &cursor.regs,
                symbolication.as_ref(),
                status,
                return_address,
            );
            lookups.resize(frames.len(), lookup);

            let Some(step) = next else {
                break;
            };
            status = step.status;
            let mut regs = step.next;
            // Unknown registers read as zero rather than as a callee's stale value.
            for (index, slot) in regs.general.iter_mut().enumerate().take(Arm64::GENERAL_REGISTER_COUNT) {
                if step.known & (1 << index) == 0 {
                    *slot = 0;
                }
            }
            cursor = Cursor {
                regs,
                known: step.known,
                // The frame a signal interrupted is stopped at an instruction, not at a
                // return address.
                is_caller: !interrupted,
            };
            depth += 1;
        }

        Ok((frames, lookups))
    }

    /// The address used for symbolication and unwind lookup: the call instruction for a
    /// caller frame, the pc itself for the innermost frame and for a frame a signal
    /// interrupted.
    fn lookup_address(&self, cursor: &Cursor) -> Address
    {
        if cursor.is_caller {
            let back = match self.arch.architecture() {
                Architecture::Arm64 => 4,
                _ => 1,
            };
            Address::new(cursor.regs.pc.value().saturating_sub(back))
        } else {
            cursor.regs.pc
        }
    }

    /// Find the caller of `cursor`'s frame, or `None` at the end of the stack. The order
    /// of sources is described in the module documentation.
    fn step(&self, cursor: &Cursor) -> Option<UnwindStep>
    {
        let lookup = self.lookup_address(cursor);
        let step = match self.symbols.image_for_address(lookup) {
            Some(image) => self.image_step(&image, cursor, lookup),
            None => self.frame_pointer_step(cursor),
        }?;
        self.accept(cursor, step)
    }

    /// Steps 1-4 of the module documentation for a pc inside a loaded image.
    fn image_step(&self, image: &BinaryImage, cursor: &Cursor, lookup: Address) -> Option<UnwindStep>
    {
        match hinted(image, lookup) {
            Hinted::Hint {
                hint: UnwindHint::Cfi { eh_frame_offset },
                ..
            } => self
                .eh_frame_fde_step(image, cursor, lookup, eh_frame_offset)
                .or_else(|| self.frame_pointer_step(cursor)),
            Hinted::Hint {
                hint: UnwindHint::Leaf { stack_size },
                at_entry,
                start,
            } => {
                // On the first instruction the leaf has not allocated its stack yet.
                if at_entry || self.is_dwarf_function_start(image, cursor) {
                    return self.frameless_step(cursor, 0);
                }
                match self.code_step(cursor, self.function_start(image, cursor, start)) {
                    Some(CodeStep::Caller(step)) => Some(step),
                    _ => self.frameless_step(cursor, stack_size),
                }
            }
            Hinted::Hint {
                hint: UnwindHint::FrameRecord,
                at_entry,
                start,
            } => {
                // A table may give one entry to a run of adjacent functions with the same
                // hint, so only the first of them is recognized from the entry; the others
                // are recognized from the DWARF function index.
                if at_entry || self.is_dwarf_function_start(image, cursor) {
                    return self.entry_step(cursor);
                }
                match self.code_step(cursor, self.function_start(image, cursor, start)) {
                    Some(CodeStep::Caller(step)) => Some(step),
                    _ => self.frame_pointer_step(cursor),
                }
            }
            Hinted::Absent => self
                .scan_cfi_step(image, cursor, lookup)
                .or_else(|| self.dwarf_entry_step(image, cursor))
                .or_else(|| self.frame_pointer_step(cursor)),
        }
    }

    /// Sanity checks every step must pass; failing one ends the trace.
    fn accept(&self, cursor: &Cursor, step: UnwindStep) -> Option<UnwindStep>
    {
        let next = &step.next;
        if next.pc == Address::ZERO || next.sp == Address::ZERO {
            return None;
        }
        // The stack grows down: a caller's frame is never below its callee's. Equal is
        // allowed only for the innermost frame (before its prologue, or frameless).
        if next.sp < cursor.regs.sp || (next.sp == cursor.regs.sp && cursor.is_caller) {
            return None;
        }
        if !self.memory.is_executable(next.pc) {
            return None;
        }
        Some(step)
    }

    fn eh_frame(image: &BinaryImage, pointer_size: u8) -> Option<(EhFrame<EndianSlice<'_, RunTimeEndian>>, BaseAddresses)>
    {
        let (vmaddr, bytes) = image.eh_frame_section()?;
        let (text_base, _) = image.runtime_range();
        let mut bases = BaseAddresses::default()
            .set_text(text_base)
            .set_eh_frame(image.relocated_address(vmaddr));
        if let Some((hdr_vmaddr, _)) = image.eh_frame_hdr_section() {
            bases = bases.set_eh_frame_hdr(image.relocated_address(hdr_vmaddr));
        }
        let mut section = EhFrame::new(bytes, image.endian());
        section.set_address_size(pointer_size);
        Some((section, bases))
    }

    /// [`UnwindHint::Cfi`]: evaluate the FDE at `fde_offset` in `eh_frame` directly, with
    /// no search.
    fn eh_frame_fde_step(
        &self,
        image: &BinaryImage,
        cursor: &Cursor,
        lookup: Address,
        fde_offset: usize,
    ) -> Option<UnwindStep>
    {
        let (section, bases) = Self::eh_frame(image, self.arch.architecture().pointer_size_bytes())?;
        let fde = section
            .partial_fde_from_offset(&bases, EhFrameOffset(fde_offset))
            .ok()?
            .parse(EhFrame::cie_from_offset)
            .ok()?;
        let mut ctx = UnwindContext::<usize>::new();
        let row = fde.unwind_info_for_address(&section, &bases, &mut ctx, lookup.value()).ok()?;
        self.apply_row(cursor, row)
    }

    /// Without a hint: `.eh_frame_hdr`, then a linear scan of `eh_frame`, then
    /// `.debug_frame`.
    ///
    /// `eh_frame` FDE ranges are runtime addresses (pc-relative, resolved through the
    /// section's runtime base); `.debug_frame` may live in an unslid companion file, so its
    /// ranges are file addresses. Each section is searched with its own address form only: trying the
    /// other one could match an unrelated function's FDE.
    fn scan_cfi_step(&self, image: &BinaryImage, cursor: &Cursor, lookup: Address) -> Option<UnwindStep>
    {
        let pointer_size = self.arch.architecture().pointer_size_bytes();
        if let Some((section, bases)) = Self::eh_frame(image, pointer_size) {
            if let Some((_, hdr_bytes)) = image.eh_frame_hdr_section()
                && let Some(step) = self.unwind_with_eh_frame_hdr(&section, hdr_bytes, &bases, image, cursor, lookup)
            {
                return Some(step);
            }
            if let Some(step) = self.unwind_with_section(&section, &bases, cursor, lookup.value()) {
                return Some(step);
            }
        }
        if let Some((_, bytes)) = image.debug_frame_section()
            && let Some(file) = image.file_address(lookup)
        {
            let bases = BaseAddresses::default();
            let mut section = DebugFrame::new(bytes, image.endian());
            section.set_address_size(pointer_size);
            if let Some(step) = self.unwind_with_section(&section, &bases, cursor, file) {
                return Some(step);
            }
        }
        None
    }

    /// Evaluate the row of the first FDE in `section` whose range contains `address`
    /// (in the section's own address form, see [`scan_cfi_step`](Self::scan_cfi_step)).
    fn unwind_with_section<R, S>(
        &self,
        section: &S,
        bases: &BaseAddresses,
        cursor: &Cursor,
        address: u64,
    ) -> Option<UnwindStep>
    where
        R: gimli::Reader<Offset = usize>,
        S: UnwindSection<R>,
    {
        let mut ctx = UnwindContext::<usize>::new();
        let mut entries = section.entries(bases);
        while let Ok(Some(entry)) = entries.next() {
            let gimli::CieOrFde::Fde(partial) = entry else {
                continue;
            };
            let Ok(fde) = partial.parse(S::cie_from_offset) else {
                continue;
            };
            if !fde.contains(address) {
                continue;
            }
            let row = fde.unwind_info_for_address(section, bases, &mut ctx, address).ok()?;
            return self.apply_row(cursor, row);
        }
        None
    }

    /// Find the FDE for `lookup` through the `.eh_frame_hdr` binary-search table.
    fn unwind_with_eh_frame_hdr(
        &self,
        section: &EhFrame<EndianSlice<'_, RunTimeEndian>>,
        hdr_bytes: &[u8],
        bases: &BaseAddresses,
        image: &BinaryImage,
        cursor: &Cursor,
        lookup: Address,
    ) -> Option<UnwindStep>
    {
        let header = EhFrameHdr::new(hdr_bytes, image.endian())
            .parse(bases, self.arch.architecture().pointer_size_bytes())
            .ok()?;
        let table = header.table()?;
        let mut ctx = UnwindContext::<usize>::new();
        let row = table
            .unwind_info_for_address(section, bases, &mut ctx, lookup.value(), EhFrame::cie_from_offset)
            .ok()?;
        self.apply_row(cursor, row)
    }

    /// Build the caller's registers from a CFI row.
    ///
    /// The caller keeps only the callee-saved registers (`SameValue` by default) plus
    /// whatever the row restores; caller-saved registers become unknown, and so does any
    /// register whose rule is `Undefined`.
    fn apply_row(&self, cursor: &Cursor, row: &gimli::UnwindTableRow<usize>) -> Option<UnwindStep>
    {
        let regs = &cursor.regs;
        let cfa = match row.cfa() {
            CfaRule::RegisterAndOffset { register, offset } => {
                let base = self.register_value(cursor, *register)?;
                base.checked_add_signed(*offset)?
            }
            CfaRule::Expression(_) => return None,
        };

        let return_register = return_register(self.arch.architecture());
        let return_address = self.recover(cursor, &row.register(return_register), cfa, return_register)??;

        let mut next = regs.clone();
        let mut known = cursor.known & self.arch.callee_saved_register_mask();
        for (register, rule) in row.registers() {
            if *register == return_register {
                continue;
            }
            let Some(bit) = general_bit(*register) else {
                continue;
            };
            match self.recover(cursor, rule, cfa, *register) {
                Some(Some(value)) => {
                    self.set_register_value(&mut next, *register, value);
                    known |= bit;
                }
                _ => known &= !bit,
            }
        }
        next.sp = Address::new(cfa);
        next.pc = self.arch.strip_pointer_auth(Address::new(return_address));
        Some(UnwindStep {
            next,
            known,
            status: FrameStatus::Complete,
        })
    }

    /// Recover a register's value in the caller. `None` when the rule cannot be evaluated
    /// (memory unreadable, unsupported expression); `Some(None)` when the caller's value
    /// is unknown.
    ///
    /// For the return-address column, `SameValue` means "still in `x30`", and so does
    /// `Undefined` when the target's
    /// [`undefined_return_address`](Arch::undefined_return_address) is
    /// [`LinkRegister`](UndefinedReturnAddress::LinkRegister) (toolchains whose CIEs leave
    /// that column undefined in the initial row): before the prologue saves it the return
    /// address is the link register, if this frame's `x30` is known (the innermost frame).
    /// Under [`EndOfStack`](UndefinedReturnAddress::EndOfStack) an undefined return
    /// address ends the walk. For any other register `Undefined` means unknown.
    fn recover(&self, cursor: &Cursor, rule: &RegisterRule<usize>, cfa: u64, register: Register) -> Option<Option<u64>>
    {
        let is_return = register == return_register(self.arch.architecture());
        match rule {
            RegisterRule::Undefined
                if is_return && self.arch.undefined_return_address() == UndefinedReturnAddress::EndOfStack =>
            {
                None
            }
            RegisterRule::Undefined | RegisterRule::SameValue if is_return => {
                if cursor.known & LINK_REGISTER == 0 {
                    return None;
                }
                Some(self.arch.link_register(&cursor.regs).map(Address::value))
            }
            RegisterRule::Undefined => Some(None),
            RegisterRule::SameValue => Some(self.register_value(cursor, register)),
            RegisterRule::Offset(offset) => {
                let address = cfa.checked_add_signed(*offset)?;
                self.memory.read_u64(Address::new(address)).ok().map(Some)
            }
            RegisterRule::ValOffset(offset) => Some(cfa.checked_add_signed(*offset)),
            RegisterRule::Register(other) => Some(self.register_value(cursor, *other)),
            _ => None,
        }
    }

    /// [`UnwindHint::Leaf`]: a leaf that never saves `x30` or `fp`. In the
    /// innermost frame the caller is the link register and the caller's `sp` is `sp`
    /// plus the leaf's stack allocation. A frameless leaf makes no calls, so it cannot be a
    /// caller frame with a valid return address: as a caller (or without `x30`) the walk
    /// stops here rather than guessing through the frame-pointer chain.
    fn frameless_step(&self, cursor: &Cursor, stack_size: u64) -> Option<UnwindStep>
    {
        if cursor.is_caller || cursor.known & LINK_REGISTER == 0 {
            return None;
        }
        let link = self.arch.link_register(&cursor.regs)?;
        let mut next = cursor.regs.clone();
        next.pc = self.arch.strip_pointer_auth(link);
        next.sp = Address::new(cursor.regs.sp.value().checked_add(stack_size)?);
        Some(UnwindStep {
            next,
            known: cursor.known & self.arch.callee_saved_register_mask(),
            status: FrameStatus::CfiFallback,
        })
    }

    /// Innermost frame on a function's first instruction: nothing was pushed, so the
    /// caller is the link register with the same `sp` and `fp`.
    fn entry_step(&self, cursor: &Cursor) -> Option<UnwindStep>
    {
        if cursor.is_caller || cursor.known & LINK_REGISTER == 0 {
            return None;
        }
        let link = self.arch.link_register(&cursor.regs)?;
        let mut next = cursor.regs.clone();
        next.pc = self.arch.strip_pointer_auth(link);
        Some(UnwindStep {
            next,
            known: cursor.known & self.arch.callee_saved_register_mask(),
            status: FrameStatus::CfiFallback,
        })
    }

    /// [`entry_step`](Self::entry_step) when the image has no hint: the entry is
    /// recognized from the DWARF function index.
    fn dwarf_entry_step(&self, image: &BinaryImage, cursor: &Cursor) -> Option<UnwindStep>
    {
        if !self.is_dwarf_function_start(image, cursor) {
            return None;
        }
        self.entry_step(cursor)
    }

    /// Whether the innermost frame's pc is the first instruction of a function in the
    /// image's DWARF function index. Always `false` for a caller frame.
    fn is_dwarf_function_start(&self, image: &BinaryImage, cursor: &Cursor) -> bool
    {
        if cursor.is_caller {
            return false;
        }
        let Some(file) = image.file_address(cursor.regs.pc) else {
            return false;
        };
        image
            .function_index()
            .is_ok_and(|index| index.starts().binary_search(&file).is_ok())
    }

    /// The start of the function containing the innermost frame's pc: the later of the
    /// hint's first function (`entry_start`) and the nearest DWARF function start at or
    /// below the pc, since a table may merge adjacent functions into one entry.
    fn function_start(&self, image: &BinaryImage, cursor: &Cursor, entry_start: Address) -> Address
    {
        let dwarf = image.file_address(cursor.regs.pc).and_then(|file| {
            let index = image.function_index().ok()?;
            let starts = index.starts();
            let at = starts.partition_point(|&start| start <= file).checked_sub(1)?;
            image.runtime_address(*starts.get(at)?)
        });
        dwarf.filter(|&start| start > entry_start).unwrap_or(entry_start)
    }

    /// The innermost frame's caller from the instructions of its function (see the
    /// module documentation, *Prologues and epilogues*): the epilogue from the pc to the
    /// `ret`, else the prologue from `start` to the pc. `None` for a caller frame, a
    /// non-arm64 target, or code that is not understood.
    fn code_step(&self, cursor: &Cursor, start: Address) -> Option<CodeStep>
    {
        if cursor.is_caller || cursor.known != ALL_GENERAL || self.arch.architecture() != Architecture::Arm64 {
            return None;
        }
        let regs = &cursor.regs;
        let mut x = [0; Arm64::GENERAL_REGISTER_COUNT];
        for (slot, value) in x.iter_mut().zip(&regs.general) {
            *slot = *value;
        }
        x[Arm64::FRAME_POINTER_INDEX] = regs.fp.value();
        let current = FrameRegisters {
            x,
            sp: regs.sp.value(),
            known: ALL_GENERAL,
        };
        let read_code = |address| self.memory.read_u32(Address::new(address)).ok();
        let read_stack = |address| self.memory.read_u64(Address::new(address)).ok();
        let pc = regs.pc.value();
        let caller = match arm64_frame::epilogue(pc, &current, read_code, read_stack) {
            Some(caller) => caller,
            None => match arm64_frame::prologue(start.value(), pc, &current, read_code, read_stack)? {
                Prologue::FrameBuilt => return Some(CodeStep::FrameBuilt),
                Prologue::Caller(caller) => *caller,
            },
        };
        if caller.known & LINK_REGISTER == 0 {
            return None;
        }
        let mut next = regs.clone();
        for (slot, value) in next.general.iter_mut().zip(caller.x) {
            *slot = value;
        }
        self.arch
            .set_fp(&mut next, Address::new(caller.x[Arm64::FRAME_POINTER_INDEX]));
        next.sp = Address::new(caller.sp);
        next.pc = self
            .arch
            .strip_pointer_auth(Address::new(caller.x[Arm64::LINK_REGISTER_INDEX]));
        Some(CodeStep::Caller(UnwindStep {
            next,
            known: caller.known & self.arch.callee_saved_register_mask(),
            status: FrameStatus::CfiFallback,
        }))
    }

    /// Follow the frame record at `fp`: `[fp]` is the caller's `fp`, `[fp + 8]` the return
    /// address.
    fn frame_pointer_step(&self, cursor: &Cursor) -> Option<UnwindStep>
    {
        let regs = &cursor.regs;
        if cursor.known & FRAME_POINTER == 0 {
            return None;
        }
        let fp = regs.fp.value();
        if fp == 0 || !fp.is_multiple_of(8) || fp < regs.sp.value() {
            return None;
        }
        let saved_fp = self.memory.read_u64(Address::new(fp)).ok()?;
        let saved_lr = self.memory.read_u64(Address::new(fp.checked_add(8)?)).ok()?;
        // The chain must move up the stack; zero marks its end (checked on the next step).
        if saved_fp != 0 && saved_fp <= fp {
            return None;
        }
        let mut next = regs.clone();
        self.arch.set_fp(&mut next, Address::new(saved_fp));
        next.sp = Address::new(fp + 16);
        next.pc = self.arch.strip_pointer_auth(Address::new(saved_lr));
        // Registers saved below the frame record are not described, so only `fp` is known
        // beyond what the callee left untouched; a frame-record function may have saved and
        // reused x19-x28, so they are unknown too.
        Some(UnwindStep {
            next,
            known: FRAME_POINTER,
            status: FrameStatus::CfiFallback,
        })
    }

    /// A caller frame in the platform's signal trampoline: the frame the signal
    /// interrupted, with every register restored from the state the kernel saved (see
    /// [Signal handler frames](self#signal-handler-frames)). `None` for any other frame.
    fn signal_frame_step(&self, cursor: &Cursor) -> Option<UnwindStep>
    {
        if !cursor.is_caller || self.arch.architecture() != Architecture::Arm64 {
            return None;
        }
        let frame = TrampolineFrame {
            architecture: Architecture::Arm64,
            pc: cursor.regs.pc,
            sp: cursor.regs.sp,
            fp: (cursor.known & FRAME_POINTER != 0).then_some(cursor.regs.fp),
        };
        let context = self.runtime.signal_context(self.memory, &frame)?;
        self.restore_signal_context(cursor, &context)
    }

    /// The arm64 interrupted frame of `context`, checked as the module documentation says.
    fn restore_signal_context(&self, cursor: &Cursor, context: &SignalContext) -> Option<UnwindStep>
    {
        if context.general.len() < Arm64::FRAME_POINTER_INDEX {
            return None;
        }
        let sp = context.sp;
        // On the interrupted stack the saved state lies below the interrupted `sp`. On an
        // alternate signal stack the interrupted `sp` is on another stack, and may be below
        // the handler's frames; it can never be among them, from the trampoline's `sp` to
        // the end of the saved state.
        let on_alternate_stack = if sp >= context.frame_end.value() {
            false
        } else if sp < cursor.regs.sp.value() {
            true
        } else {
            return None;
        };

        let mut next = cursor.regs.clone();
        for (slot, &value) in next.general.iter_mut().zip(&context.general).take(Arm64::FRAME_POINTER_INDEX) {
            *slot = value;
        }
        self.arch.set_fp(&mut next, Address::new(context.fp));
        if let Some(slot) = next.general.get_mut(Arm64::LINK_REGISTER_INDEX) {
            *slot = context.lr;
        }
        next.sp = Address::new(sp);
        next.pc = self.arch.strip_pointer_auth(Address::new(context.pc));
        let step = UnwindStep {
            next,
            known: ALL_GENERAL,
            status: FrameStatus::Complete,
        };
        if on_alternate_stack {
            self.accept_alternate_stack(step)
        } else {
            self.accept(cursor, step)
        }
    }

    /// The checks of [`accept`](Self::accept) for the one step that may move the stack
    /// pointer down: from a signal trampoline on an alternate signal stack to the frame the
    /// signal interrupted. The interrupted state comes from a validated context, so instead of
    /// `sp` growing, its `sp` must be a non-zero, 16-byte aligned (arm64 requires it)
    /// address and its pc executable. Later steps compare against the interrupted `sp`.
    fn accept_alternate_stack(&self, step: UnwindStep) -> Option<UnwindStep>
    {
        let next = &step.next;
        if next.pc == Address::ZERO || next.sp == Address::ZERO || !next.sp.value().is_multiple_of(16) {
            return None;
        }
        if !self.memory.is_executable(next.pc) {
            return None;
        }
        Some(step)
    }

    fn register_value(&self, cursor: &Cursor, register: Register) -> Option<u64>
    {
        if let Some(bit) = general_bit(register)
            && self.arch.architecture() == Architecture::Arm64
            && cursor.known & bit == 0
        {
            return None;
        }
        read_register_value(self.arch.architecture(), &cursor.regs, register)
    }

    fn set_register_value(&self, regs: &mut Registers, register: Register, value: u64)
    {
        if self.arch.architecture() != Architecture::Arm64 {
            return;
        }
        match usize::from(register.0) {
            Arm64::FRAME_POINTER_INDEX => self.arch.set_fp(regs, Address::new(value)),
            Arm64::STACK_POINTER_DWARF_REGISTER => regs.sp = Address::new(value),
            n @ 0..Arm64::GENERAL_REGISTER_COUNT => {
                if let Some(slot) = regs.general.get_mut(n) {
                    *slot = value;
                }
            }
            _ => {}
        }
    }
}

/// The `known` bit of an arm64 DWARF register number `x0`-`x30`.
fn general_bit(register: Register) -> Option<u32>
{
    (usize::from(register.0) < Arm64::GENERAL_REGISTER_COUNT).then(|| 1 << register.0)
}

/// The unwind hint of the function containing `lookup`, if the image has one.
fn hinted(image: &BinaryImage, lookup: Address) -> Hinted
{
    match image.unwind_hints().and_then(|hints| hints.lookup(lookup)) {
        Some(entry) => Hinted::Hint {
            hint: entry.hint,
            at_entry: entry.start == lookup,
            start: entry.start,
        },
        None => Hinted::Absent,
    }
}

/// Push one physical frame and the frames inlined into it.
///
/// `symbolication.frames` comes from `addr2line`, innermost first: the inlined callees,
/// then the function that owns the code. Each inlined function is its own logical frame
/// (kind [`FrameKind::Inlined`]) before the physical frame.
#[allow(clippy::too_many_arguments)]
fn append_logical_frames(
    frames: &mut Vec<StackFrame>,
    max_frames: usize,
    thread: ThreadId,
    depth: u32,
    regs: &Registers,
    symbolication: Option<&Symbolication>,
    status: FrameStatus,
    return_address: Option<Address>,
)
{
    let physical_id = FrameId::new(thread, depth, 0, regs.pc, regs.sp);
    let symbol_frames: &[SymbolFrame] = symbolication.map_or(&[], |symbols| symbols.frames.as_slice());
    let frame = |kind, symbol_frame: Option<&SymbolFrame>, id, index| StackFrame {
        id,
        thread,
        index,
        kind,
        pc: regs.pc,
        sp: regs.sp,
        fp: regs.fp,
        return_address,
        symbol: symbol_frame.map(|f| f.symbol.clone()),
        location: symbol_frame.and_then(|f| f.location.clone()),
        parameters: symbol_frame.map(|f| f.parameters.clone()).unwrap_or_default(),
        status,
    };

    let (physical, inlined) = match symbol_frames.split_last() {
        Some((physical, inlined)) => (Some(physical), inlined),
        None => (None, &[][..]),
    };
    for (inline_depth, symbol_frame) in inlined.iter().enumerate() {
        if frames.len() >= max_frames {
            return;
        }
        let inline_depth = u8::try_from(inline_depth).unwrap_or(u8::MAX);
        let id = FrameId::new(thread, depth, inline_depth.saturating_add(1), regs.pc, regs.sp);
        let kind = FrameKind::Inlined {
            physical: physical_id,
            depth: inline_depth,
        };
        frames.push(frame(kind, Some(symbol_frame), id, frames.len()));
    }
    if frames.len() < max_frames {
        frames.push(frame(FrameKind::Physical, physical, physical_id, frames.len()));
    }
}

fn read_register_value(architecture: Architecture, regs: &Registers, register: Register) -> Option<u64>
{
    let reg_num = usize::from(register.0);
    match architecture {
        Architecture::Arm64 => match reg_num {
            Arm64::STACK_POINTER_DWARF_REGISTER => Some(regs.sp.value()),
            Arm64::FRAME_POINTER_INDEX => Some(regs.fp.value()),
            0..Arm64::GENERAL_REGISTER_COUNT => regs.general.get(reg_num).copied(),
            _ => None,
        },
        Architecture::X86_64 => match reg_num {
            7 => Some(regs.sp.value()),
            16 => Some(regs.pc.value()),
            6 => Some(regs.fp.value()),
            0..=15 => {
                let mapping = [0, 2, 1, 3, 6, 7, 5, 4, 8, 9, 10, 11, 12, 13, 14, 15];
                mapping.get(reg_num).and_then(|&idx| regs.general.get(idx).copied())
            }
            _ => None,
        },
        Architecture::Unknown(_) => None,
    }
}

fn return_register(architecture: Architecture) -> Register
{
    match architecture {
        Architecture::X86_64 => Register(16),
        _ => Register(Arm64::LINK_REGISTER_INDEX as u16),
    }
}

#[cfg(test)]
mod tests
{
    use std::collections::HashMap;

    use super::*;
    use crate::backend::mock::arm64_registers;
    use crate::error::DebuggerError;
    use crate::runtime::mock::MockRuntime;

    /// Code of the mock signal trampoline the tests' runtime recognizes.
    static RUNTIME: std::sync::LazyLock<MockRuntime> =
        std::sync::LazyLock::new(|| MockRuntime::new().with_signal_trampoline(0x5000_0000..0x5000_0100));

    /// Little-endian words at addresses; anything else is unreadable.
    #[derive(Default)]
    struct FakeMemory
    {
        words: HashMap<u64, u64>,
        code: Option<(u64, u64)>,
    }

    impl MemoryAccess for FakeMemory
    {
        fn read_u64(&self, address: Address) -> Result<u64>
        {
            self.words
                .get(&address.value())
                .copied()
                .ok_or_else(|| DebuggerError::InvalidArgument(format!("unreadable {address}")))
        }

        fn is_executable(&self, address: Address) -> bool
        {
            self.code.is_none_or(|(start, end)| (start..end).contains(&address.value()))
        }
    }

    static ARCH: Arm64 = Arm64::new();

    fn regs(pc: u64, sp: u64, fp: u64, lr: u64) -> Registers
    {
        let mut regs = arm64_registers();
        regs.pc = Address::new(pc);
        regs.sp = Address::new(sp);
        ARCH.set_fp(&mut regs, Address::new(fp));
        ARCH.set_link_register(&mut regs, Address::new(lr)).expect("lr");
        regs
    }

    fn pcs(memory: &FakeMemory, regs: &Registers) -> Vec<u64>
    {
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        StackUnwinder::new(&ARCH, &symbols, memory)
            .with_runtime(&*RUNTIME)
            .unwind(ThreadId::from(1), regs, 64)
            .expect("unwind")
            .iter()
            .map(|frame| frame.pc.value())
            .collect()
    }

    /// Three frame records; the outermost has a zero fp, which ends the chain.
    fn chain() -> FakeMemory
    {
        let mut memory = FakeMemory::default();
        memory.words.insert(0x1000, 0x1100);
        memory.words.insert(0x1008, 0x4000_0010);
        memory.words.insert(0x1100, 0x1200);
        memory.words.insert(0x1108, 0x4000_0020);
        memory.words.insert(0x1200, 0);
        memory.words.insert(0x1208, 0x4000_0030);
        memory
    }

    #[test]
    fn frame_pointer_chain_ends_at_a_zero_fp()
    {
        let memory = chain();
        assert_eq!(
            pcs(&memory, &regs(0x4000_0000, 0xf00, 0x1000, 0x4000_0abc)),
            [0x4000_0000, 0x4000_0010, 0x4000_0020, 0x4000_0030]
        );
    }

    #[test]
    fn link_register_is_never_used_for_caller_frames()
    {
        // No frame pointer and no CFI: the old link-register fallback produced the LR as a
        // caller and then the LR again at every depth. Now the trace is just frame 0.
        let memory = FakeMemory::default();
        assert_eq!(pcs(&memory, &regs(0x4000_0000, 0xf00, 0, 0x4000_0abc)), [0x4000_0000]);
    }

    #[test]
    fn no_stack_scan_on_an_unreadable_frame_record()
    {
        // The old stack scan read [sp] as a return address when fp was unusable.
        let mut memory = FakeMemory::default();
        memory.words.insert(0xf00, 0x4000_0500);
        assert_eq!(pcs(&memory, &regs(0x4000_0000, 0xf00, 0x2000, 0)), [0x4000_0000]);
    }

    #[test]
    fn pointer_auth_bits_are_stripped_from_saved_return_addresses()
    {
        let mut memory = chain();
        memory.words.insert(0x1008, 0x8012_0000_4000_0010);
        let trace = pcs(&memory, &regs(0x4000_0000, 0xf00, 0x1000, 0));
        assert_eq!(trace[1], 0x4000_0010);
    }

    #[test]
    fn a_decreasing_frame_chain_stops_the_trace()
    {
        let mut memory = chain();
        memory.words.insert(0x1100, 0x1000);
        assert_eq!(pcs(&memory, &regs(0x4000_0000, 0xf00, 0x1000, 0)), [0x4000_0000, 0x4000_0010]);
    }

    #[test]
    fn a_return_address_outside_code_stops_the_trace()
    {
        let mut memory = chain();
        memory.code = Some((0x4000_0000, 0x4000_0021));
        assert_eq!(
            pcs(&memory, &regs(0x4000_0000, 0xf00, 0x1000, 0)),
            [0x4000_0000, 0x4000_0010, 0x4000_0020]
        );
    }

    fn innermost(regs: Registers) -> Cursor
    {
        Cursor {
            regs,
            known: ALL_GENERAL,
            is_caller: false,
        }
    }

    #[test]
    fn frameless_leaf_returns_through_the_link_register_only_when_innermost()
    {
        let memory = FakeMemory::default();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory).with_runtime(&*RUNTIME);
        let mut regs = regs(0x4000_0000, 0xf00, 0x1000, 0x8012_0000_4000_0abc);
        regs.general[3] = 33;
        regs.general[20] = 20;
        let step = unwinder
            .frameless_step(&innermost(regs.clone()), 32)
            .expect("innermost frameless");
        assert_eq!(step.next.pc, Address::new(0x4000_0abc), "PAC bits stripped");
        assert_eq!(step.next.sp, Address::new(0xf20));
        assert_eq!(step.next.fp, Address::new(0x1000), "a leaf leaves fp alone");
        assert_eq!(step.known & (1 << 3), 0, "x3 is caller-saved");
        assert_ne!(step.known & (1 << 20), 0, "x20 is callee-saved");
        assert_eq!(step.known & LINK_REGISTER, 0);

        let caller = Cursor {
            is_caller: true,
            ..innermost(regs)
        };
        assert!(unwinder.frameless_step(&caller, 32).is_none(), "a leaf is never a caller");
    }

    #[test]
    fn unknown_registers_cannot_feed_a_rule()
    {
        let memory = FakeMemory::default();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory).with_runtime(&*RUNTIME);
        let cursor = Cursor {
            known: ARCH.callee_saved_register_mask(),
            is_caller: true,
            ..innermost(regs(0x4000_0000, 0xf00, 0x1000, 0x4000_0abc))
        };
        let lr = Register(30);
        assert_eq!(unwinder.recover(&cursor, &RegisterRule::SameValue, 0, lr), None);
        assert_eq!(unwinder.recover(&cursor, &RegisterRule::Undefined, 0, lr), None);
        assert_eq!(
            unwinder.recover(&cursor, &RegisterRule::Undefined, 0, Register(20)),
            Some(None)
        );
        assert_eq!(
            unwinder.recover(&cursor, &RegisterRule::SameValue, 0, Register(2)),
            Some(None)
        );
        assert_eq!(
            unwinder.recover(&cursor, &RegisterRule::SameValue, 0, Register(29)),
            Some(Some(0x1000))
        );
    }

    /// #222: the meaning of an undefined return-address column is the target's: the
    /// link register by default, the end of the stack where the target says so.
    #[test]
    fn an_undefined_return_address_follows_the_configured_rule()
    {
        let memory = FakeMemory::default();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let cursor = innermost(regs(0x4000_0000, 0xf00, 0x1000, 0x4000_0abc));
        let lr = Register(30);
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory);
        assert_eq!(
            unwinder.recover(&cursor, &RegisterRule::Undefined, 0, lr),
            Some(Some(0x4000_0abc))
        );
        let end_of_stack = Arm64::new().with_undefined_return_address(UndefinedReturnAddress::EndOfStack);
        let unwinder = StackUnwinder::new(&end_of_stack, &symbols, &memory);
        assert_eq!(unwinder.recover(&cursor, &RegisterRule::Undefined, 0, lr), None);
        assert_eq!(
            unwinder.recover(&cursor, &RegisterRule::SameValue, 0, lr),
            Some(Some(0x4000_0abc)),
            "SameValue still means the link register"
        );
    }

    /// A handler frame record at 0x1000 returning into the mock trampoline at 0x5000_0044
    /// whose frame record (0x1100) sits right below its saved state; the interrupted frame
    /// is at pc 0x4000_0100 with sp 0x2000, fp 0x2100, lr 0x4000_0aa0, and its own frame
    /// record at 0x2100 returns to 0x4000_0200 (fp chain end).
    fn signal_stack() -> FakeMemory
    {
        let mut memory = FakeMemory::default();
        memory.words.insert(0x1000, 0x1100);
        memory.words.insert(0x1008, 0x5000_0044);
        // The trampoline's frame record: the interrupted fp and x30, not a caller of it.
        memory.words.insert(0x1100, 0x2100);
        memory.words.insert(0x1108, 0x4000_0aa0);
        let context = 0x1110;
        for index in 0..29 {
            memory.words.insert(context + 8 * index, 0x100 + index);
        }
        memory.words.insert(context + MockRuntime::FP, 0x2100);
        memory.words.insert(context + MockRuntime::LR, 0x4000_0aa0);
        memory.words.insert(context + MockRuntime::SP, 0x2000);
        memory.words.insert(context + MockRuntime::PC, 0x8012_0000_4000_0100);
        memory.words.insert(0x2100, 0);
        memory.words.insert(0x2108, 0x4000_0200);
        memory
    }

    #[test]
    fn a_signal_frame_restores_the_interrupted_frame()
    {
        let memory = signal_stack();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .with_runtime(&*RUNTIME)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0xf00, 0x1000, 0), 64)
            .expect("unwind");
        let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
        assert_eq!(pcs, [0x4000_0000, 0x5000_0044, 0x4000_0100, 0x4000_0200]);
        assert_eq!(frames[2].sp, Address::new(0x2000));
        assert_eq!(frames[2].fp, Address::new(0x2100));
        assert_eq!(frames[2].status, FrameStatus::Complete);
        assert_eq!(frames[1].return_address, Some(Address::new(0x4000_0100)));
    }

    #[test]
    fn the_interrupted_frame_keeps_every_register_and_is_not_a_caller()
    {
        let memory = signal_stack();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory).with_runtime(&*RUNTIME);
        let trampoline = Cursor {
            known: 1 << 29,
            is_caller: true,
            ..innermost(regs(0x5000_0044, 0x10f0, 0x1100, 0))
        };
        let step = unwinder.signal_frame_step(&trampoline).expect("a signal frame");
        assert_eq!(step.known, ALL_GENERAL);
        assert_eq!(step.next.general[0], 0x100);
        assert_eq!(step.next.general[28], 0x100 + 28);
        assert_eq!(ARCH.link_register(&step.next), Some(Address::new(0x4000_0aa0)));
        assert_eq!(step.next.pc, Address::new(0x4000_0100), "PAC bits stripped");
        // Looked up at its pc, like an innermost frame.
        let interrupted = Cursor {
            regs: step.next,
            known: step.known,
            is_caller: false,
        };
        assert_eq!(unwinder.lookup_address(&interrupted), Address::new(0x4000_0100));
    }

    #[test]
    fn a_context_with_too_few_registers_is_ignored()
    {
        /// A runtime that recognizes every frame and returns `general` short of `x28`.
        struct Short;
        impl TargetRuntime for Short
        {
            fn signal_context(&self, _memory: &dyn MemoryAccess, _frame: &TrampolineFrame) -> Option<SignalContext>
            {
                Some(SignalContext {
                    general: vec![0; 28],
                    fp: 0x2100,
                    lr: 0x4000_0aa0,
                    sp: 0x2000,
                    pc: 0x4000_0100,
                    frame_end: Address::new(0x1200),
                })
            }
        }
        let memory = signal_stack();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let trampoline = Cursor {
            known: 1 << 29,
            is_caller: true,
            ..innermost(regs(0x5000_0044, 0x10f0, 0x1100, 0))
        };
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .with_runtime(&Short)
                .signal_frame_step(&trampoline)
                .is_none()
        );
    }

    #[test]
    fn a_frame_without_a_signal_frame_is_not_a_trampoline()
    {
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let trampoline = Cursor {
            known: 1 << 29,
            is_caller: true,
            ..innermost(regs(0x5000_0044, 0x10f0, 0x1100, 0))
        };
        // A pc outside the runtime's trampoline.
        let memory = signal_stack();
        let elsewhere = Cursor {
            regs: regs(0x4000_0044, 0x10f0, 0x1100, 0),
            ..trampoline.clone()
        };
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .with_runtime(&*RUNTIME)
                .signal_frame_step(&elsewhere)
                .is_none()
        );
        // An unknown frame pointer.
        let unknown_fp = Cursor {
            known: 0,
            ..trampoline.clone()
        };
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .with_runtime(&*RUNTIME)
                .signal_frame_step(&unknown_fp)
                .is_none()
        );
        // Without the platform's runtime no frame is a trampoline.
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .signal_frame_step(&trampoline)
                .is_none()
        );
        // The innermost frame is never a trampoline's caller frame.
        let memory = signal_stack();
        let innermost = Cursor {
            is_caller: false,
            ..trampoline
        };
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .with_runtime(&*RUNTIME)
                .signal_frame_step(&innermost)
                .is_none()
        );
    }

    /// [`signal_stack`] with the signal frame on an alternate signal stack at 0x9000:
    /// the handler's frame record at 0x9000 returns into the trampoline, whose record at
    /// 0x9100 sits right below the signal frame. The interrupted frame (sp 0x2000, fp
    /// 0x2100) is on the thread's own stack, below every frame on the alternate stack.
    fn alternate_signal_stack() -> FakeMemory
    {
        let original = signal_stack();
        let mut memory = FakeMemory::default();
        for (&address, &word) in &original.words {
            let moved = if (0x1000..0x2000).contains(&address) {
                address + 0x8000
            } else {
                address
            };
            memory.words.insert(moved, word);
        }
        memory.words.insert(0x9000, 0x9100);
        memory
    }

    fn alternate_stack_trampoline() -> Cursor
    {
        Cursor {
            known: 1 << 29,
            is_caller: true,
            ..innermost(regs(0x5000_0044, 0x90f0, 0x9100, 0))
        }
    }

    #[test]
    fn a_signal_frame_on_an_alternate_stack_restores_the_interrupted_frame()
    {
        let memory = alternate_signal_stack();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .with_runtime(&*RUNTIME)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0x8f00, 0x9000, 0), 64)
            .expect("unwind");
        let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
        assert_eq!(pcs, [0x4000_0000, 0x5000_0044, 0x4000_0100, 0x4000_0200]);
        assert_eq!(frames[1].sp, Address::new(0x9010));
        assert_eq!(frames[2].sp, Address::new(0x2000), "sp moves down across the signal frame");
        assert_eq!(frames[2].fp, Address::new(0x2100));
        assert_eq!(frames[2].status, FrameStatus::Complete);
        assert_eq!(frames[3].sp, Address::new(0x2110), "checked against the interrupted sp");
    }

    #[test]
    fn after_an_alternate_stack_signal_frame_sp_must_grow_again()
    {
        // The interrupted frame's record links to a record below the interrupted sp
        // (0x2000): the chain is checked against the interrupted frame and ends there.
        let mut memory = alternate_signal_stack();
        memory.words.insert(0x2100, 0x1f00);
        memory.words.insert(0x1f00, 0);
        memory.words.insert(0x1f08, 0x4000_0300);
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .with_runtime(&*RUNTIME)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0x8f00, 0x9000, 0), 64)
            .expect("unwind");
        let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
        assert_eq!(pcs, [0x4000_0000, 0x5000_0044, 0x4000_0100]);
    }

    #[test]
    fn an_alternate_stack_signal_frame_still_needs_a_plausible_interrupted_state()
    {
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let context = 0x9110;
        let cursor = alternate_stack_trampoline();
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &alternate_signal_stack())
                .with_runtime(&*RUNTIME)
                .signal_frame_step(&cursor)
                .is_some()
        );
        let rejected = |change: &dyn Fn(&mut FakeMemory)| {
            let mut memory = alternate_signal_stack();
            change(&mut memory);
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .with_runtime(&*RUNTIME)
                .signal_frame_step(&cursor)
                .is_none()
        };
        // An interrupted sp among the handler's frames on the alternate stack.
        assert!(rejected(&|memory| {
            memory.words.insert(context + MockRuntime::SP, 0x9200);
        }));
        // A zero or misaligned interrupted sp.
        assert!(rejected(&|memory| {
            memory.words.insert(context + MockRuntime::SP, 0);
        }));
        assert!(rejected(&|memory| {
            memory.words.insert(context + MockRuntime::SP, 0x2008);
        }));
        // An interrupted pc outside code.
        assert!(rejected(&|memory| {
            memory.code = Some((0x4000_0000, 0x4000_0100));
        }));
        // An unreadable context.
        assert!(rejected(&|memory| {
            memory.words.remove(&(context + 8 * 7));
        }));
    }

    #[test]
    fn a_corrupt_alternate_stack_context_cannot_loop_the_trace()
    {
        // The saved context points back at the handler's own frame on the alternate
        // stack, so taking it again and again would repeat the same frames until
        // `max_frames`. Only the first downward signal step is taken.
        let mut memory = alternate_signal_stack();
        let context = 0x9110;
        memory.words.insert(context + MockRuntime::SP, 0x8f00);
        memory.words.insert(context + MockRuntime::FP, 0x9000);
        memory.words.insert(context + MockRuntime::PC, 0x4000_0000);
        let trace = pcs(&memory, &regs(0x4000_0000, 0x8f00, 0x9000, 0));
        assert!(trace.len() < 8, "the trace loops: {trace:x?}");
        assert_eq!(
            trace.iter().filter(|&&pc| pc == 0x5000_0044).count(),
            2,
            "the trampoline is reached at most twice: {trace:x?}"
        );
    }

    #[test]
    fn a_decreasing_sp_is_rejected_on_any_step_but_the_signal_frame()
    {
        // The same downward move as the alternate-stack signal step (from 0x9000 to the
        // 0x1f00 region), through a frame record instead: the record at 0x9000 is not
        // followed.
        let mut memory = FakeMemory::default();
        memory.words.insert(0x8f00, 0x9000);
        memory.words.insert(0x8f08, 0x4000_0010);
        memory.words.insert(0x9000, 0x1f00);
        memory.words.insert(0x9008, 0x4000_0020);
        memory.words.insert(0x1f00, 0);
        memory.words.insert(0x1f08, 0x4000_0030);
        assert_eq!(
            pcs(&memory, &regs(0x4000_0000, 0x8e00, 0x8f00, 0)),
            [0x4000_0000, 0x4000_0010]
        );

        // Any other step that would restore that state (a CFI row, for example) is
        // rejected by `accept`, which the signal step bypasses only on an alternate stack.
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let memory = alternate_signal_stack();
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory).with_runtime(&*RUNTIME);
        let caller = alternate_stack_trampoline();
        let signal = unwinder.signal_frame_step(&caller).expect("the alternate-stack signal step");
        assert_eq!(signal.next.sp, Address::new(0x2000));
        assert!(
            unwinder.accept(&caller, signal).is_none(),
            "accept keeps sp monotonic for every other step"
        );
    }

    /// Link-time and load address of [`cfi_image`]'s one segment, and the one function
    /// its `eh_frame` describes: CFA `x29 + 16`, `x29` at CFA - 16, `x30` at CFA - 8.
    const CFI_BASE: u64 = 0x1_0000_0000;
    const CFI_FUNCTION: u64 = CFI_BASE + 0x1000;

    /// A neutral mock image with `eh_frame` and no unwind hints (so CFI is used), plus
    /// placeholder DWARF.
    fn cfi_image() -> Vec<u8>
    {
        fn u32s(bytes: &mut Vec<u8>, values: &[u32])
        {
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        const EH_FRAME: u64 = CFI_BASE + 0x400;

        let mut eh_frame = Vec::new();
        // CIE `zR`, pcrel|sdata4 pointers, code alignment 4, data alignment -8, RA x30,
        // initial rule CFA = sp.
        u32s(&mut eh_frame, &[20, 0]);
        eh_frame.extend_from_slice(&[1, b'z', b'R', 0, 4, 0x78, 30, 1, 0x1b, 0x0c, 31, 0, 0, 0, 0, 0]);
        // FDE at offset 24 for [CFI_FUNCTION, +0x100).
        let pc_begin_field = EH_FRAME + 24 + 8;
        let pc_begin = i32::try_from(CFI_FUNCTION.cast_signed() - pc_begin_field.cast_signed()).unwrap();
        u32s(&mut eh_frame, &[20, 28, pc_begin.cast_unsigned(), 0x100]);
        // def_cfa x29, 16; offset x30 at cfa - 8; offset x29 at cfa - 16.
        eh_frame.extend_from_slice(&[0, 0x0c, 29, 16, 0x9e, 1, 0x9d, 2]);
        u32s(&mut eh_frame, &[0]);

        crate::objfile::mock::MockObjectFile::new(CFI_BASE, 0x1_0000)
            .with_eh_frame(EH_FRAME, &eh_frame)
            .with_dwarf(gimli::SectionId::DebugInfo, &[0xaa; 8])
            .with_dwarf(gimli::SectionId::DebugAbbrev, &[0xaa; 8])
            .with_dwarf(gimli::SectionId::DebugLine, &[0xaa; 8])
            .to_bytes()
    }

    #[test]
    fn after_an_alternate_stack_signal_frame_a_cfi_step_must_grow_sp()
    {
        // Issue #134: the interrupted frame (sp 0x2000) is in a function with CFI whose row
        // restores sp 0x1f10 (x29 0x1f00 + 16), below the interrupted sp. The frame record
        // and return address it reads are valid, so only `accept`'s sp check ends the trace.
        let path = std::env::temp_dir().join(format!("ferros-unwind-cfi-sp-{}.mock", std::process::id()));
        std::fs::write(&path, cfi_image()).expect("write synthetic image");
        let mut symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let loaded = symbols.load_image(crate::symbols::ImageDescriptor {
            path: path.clone(),
            load_address: CFI_BASE,
        });
        let _ = std::fs::remove_file(&path);
        loaded.expect("load synthetic image");

        let mut memory = alternate_signal_stack();
        let context = 0x9110;
        let interrupted_pc = CFI_FUNCTION + 0x10;
        memory.words.insert(context + MockRuntime::PC, interrupted_pc);
        memory.words.insert(context + MockRuntime::FP, 0x1f00);
        memory.words.insert(0x1f00, 0);
        memory.words.insert(0x1f08, 0x4000_0300);
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .with_runtime(&*RUNTIME)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0x8f00, 0x9000, 0), 64)
            .expect("unwind");
        let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
        assert_eq!(pcs, [0x4000_0000, 0x5000_0044, interrupted_pc]);
        assert_eq!(frames[2].sp, Address::new(0x2000));

        // The rejected step is the CFI row, not the frame-pointer fallback.
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory).with_runtime(&*RUNTIME);
        let interrupted = Cursor {
            regs: regs(interrupted_pc, 0x2000, 0x1f00, 0x4000_0aa0),
            known: ALL_GENERAL,
            is_caller: false,
        };
        let image = symbols.image_for_address(Address::new(interrupted_pc)).expect("image");
        let row = unwinder
            .scan_cfi_step(&image, &interrupted, Address::new(interrupted_pc))
            .expect("a CFI step");
        assert_eq!(row.next.sp, Address::new(0x1f10));
        assert_eq!(row.status, FrameStatus::Complete);
        assert!(unwinder.accept(&interrupted, row).is_none());
    }

    #[test]
    fn max_frames_limits_the_trace()
    {
        let memory = chain();
        let symbols = SymbolCache::new(crate::objfile::mock::MockObjectFormat::shared());
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .with_runtime(&*RUNTIME)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0xf00, 0x1000, 0), 2)
            .expect("unwind");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].index, 1);
        assert_eq!(frames[0].return_address, Some(Address::new(0x4000_0010)));
    }
}
