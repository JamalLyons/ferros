//! # Stack Unwinding
//!
//! Walks a stopped thread's stack from its registers, innermost frame first.
//!
//! ## Unwind sources and their order
//!
//! Rust binaries built for `aarch64-apple-darwin` carry these unwind sources (checked on
//! a debug build of the `nested_calls` fixture with `xcrun dwarfdump --eh-frame
//! --debug-frame` and `objdump --unwind-info`):
//!
//! - `__TEXT,__unwind_info` (compact unwind) has an entry for every function: 257 with
//!   `UNWIND_ARM64_MODE_DWARF` (0x03, the low 24 bits are the offset of the function's
//!   FDE in `__eh_frame`), 77 `UNWIND_ARM64_MODE_FRAMELESS` (0x02, leaves with no FDE, such
//!   as `__rust_alloc`), 21 `UNWIND_ARM64_MODE_FRAME` (0x04, a frame record at `x29` and
//!   no FDE, such as the C `_main` shim), 5 with encoding 0, and 134 DWARF-mode entries
//!   with personality/LSDA bits. Rust functions, including the fixture's own `main`, are
//!   DWARF mode.
//! - `__TEXT,__eh_frame` holds the FDEs the DWARF-mode entries point to. Their rows
//!   describe the prologue exactly, including the first instruction (`CFA=sp`, return
//!   address still in `x30`).
//! - `.debug_frame` in the dSYM is empty.
//! - Frameless leaves do not set up `x29`: there `fp` is still the caller's, so following
//!   the frame-pointer chain from such a leaf skips its caller.
//!
//! Each step looks up the compact entry of the function containing the lookup address
//! (the pc of the innermost frame, the call instruction of a caller) and uses:
//!
//! 1. **`MODE_DWARF`**: the FDE at the encoded offset in `__eh_frame`, evaluated directly
//!    (no search: the main `ferros` binary has about 34,000 FDEs). If it cannot be
//!    evaluated, the frame-pointer chain.
//! 2. **`MODE_FRAMELESS`**: in the innermost frame, the caller is the link register and
//!    its `sp` is `sp + stack_size` (`sp` itself on the first instruction, before the
//!    allocation). Elsewhere the instructions are analysed first (see *Prologues and
//!    epilogues* below), so a stop after the epilogue released the allocation gets the
//!    caller's `sp` right. A frameless leaf makes no calls, so it cannot be a caller frame;
//!    there the walk stops instead of guessing.
//! 3. **`MODE_FRAME`**: the frame-pointer chain: the record at `fp` holds the caller's
//!    `fp` and the return address. On the function's first instruction (innermost frame
//!    only) the record is not built yet, so the caller is the link register. The linker
//!    merges a run of adjacent functions with the same encoding into one entry (33 such
//!    function starts under a `MODE_FRAME` entry in this build), so a first instruction is
//!    recognized from the entry or from the DWARF function index. Elsewhere in the
//!    innermost frame the instructions are analysed first (see *Prologues and epilogues*),
//!    which covers the rest of the prologue, the epilogue after `x29`/`x30` are reloaded,
//!    and a shrink-wrapped path before the record is built.
//! 4. **Encoding 0, no entry, or no `__unwind_info`** (for example an ELF image or a
//!    synthetic one): `.eh_frame_hdr` lookup, else a linear scan of `__eh_frame` (FDE
//!    ranges as runtime addresses), then of `.debug_frame` (FDE ranges as file addresses,
//!    since it lives in the unslid dSYM); then, in the innermost frame on the first
//!    instruction of a function known to DWARF, the link register; then the frame-pointer
//!    chain.
//!
//! ## Prologues and epilogues
//!
//! Compact unwind cannot describe the instructions of a `MODE_FRAME` function before
//! `x29` is set to its frame record or after the epilogue reloads `x29`/`x30`, nor a
//! `MODE_FRAMELESS` function after its epilogue released the stack. For an innermost
//! frame in those modes, past the first instruction, the unwinder decodes the code around
//! the pc ([`arch::arm64_frame`](crate::arch::arm64_frame)), read with
//! [`MemoryAccess::read_u32`]:
//!
//! 1. **Epilogue**: from the pc forward, the straight-line code to the `ret` (`ret`,
//!    `retaa`, `retab`) is executed on the instructions that move `sp` or reload
//!    registers from the stack. The registers at the `ret` are the caller's: the return
//!    address in `x30`, `sp`, `x29` and the reloaded `x19`-`x28`.
//! 2. **Prologue**: otherwise, from the function's start up to the pc. The start is the
//!    later of the compact entry's function and the nearest DWARF function start (the
//!    linker merges entries). If an instruction sets `x29` from `sp`, the record is built
//!    and the frame-pointer chain applies. Otherwise the caller's `sp` is `sp` plus what
//!    the code allocated, and `x29`, `x30`, `x19`-`x28` come from where it stored them,
//!    else they still hold the caller's values.
//! 3. If neither is understood (a branch or call on the way, an instruction that is not
//!    decoded, an unreadable word), the mode's rule above applies unchanged.
//!
//! Recognized instructions include `stp`/`ldp`/`str`/`ldr` through `sp` (offset,
//! pre-index, post-index), `sub`/`add sp, sp, #N`, `add x29, sp, #N` and `mov x29, sp`,
//! `mov sp, x29`, and hints such as `pacibsp`/`autibsp`; ordinary data processing is
//! neutral. The session's [`MemoryAccess`] hides enabled software breakpoints behind
//! their original bytes, so a stop at a breakpoint inside a prologue or epilogue still
//! decodes the real instructions.
//!
//! Images whose files could not be loaded (for example libraries in the dyld shared cache)
//! have none of this, and use the frame-pointer chain.
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
//! A caller frame keeps only what is known about it: the callee-saved `x19`-`x29` the
//! callee left alone, plus registers its CFI restores. Caller-saved registers (`x0`-`x18`,
//! and `x30` except through the return-address rule) and registers whose rule is
//! `Undefined` are unknown and read as zero. Rules that use an unknown register fail.
//!
//! ## Signal handler frames
//!
//! On arm64 Darwin the kernel delivers a signal by building a signal frame on the
//! interrupted thread's stack and starting the thread in `_sigtramp`
//! (`libsystem_platform`, in the dyld shared cache), which calls the handler. xnu's
//! `sendsig` (`bsd/dev/arm/unix_signal.c`) lays the frame out as `struct user_sigframe64`
//! at the new `sp`, 16-byte aligned below the interrupted `sp` and a 128-byte red zone:
//!
//! | offset | size | field |
//! |-------:|-----:|-------|
//! | 0      | 104  | `siginfo_t` |
//! | 104    | 56   | `ucontext_t`; `uc_mcsize` at +40 (816), `uc_mcontext` at +48 |
//! | 160    | 816  | `__darwin_mcontext64`: `__es` (16: far, esr, exception), `__ss` at +16, `__ns` at +288 |
//!
//! `__ss` is the interrupted thread state: `x0`-`x28` at +0, `fp` at +232, `lr` at +240,
//! `sp` at +248, `pc` at +256, `cpsr` at +264. The kernel passes the handler, `siginfo`
//! and `ucontext` pointers in `x0`, `x3` and `x4`, and leaves every other register,
//! `x29` and `x30` included, as the interrupted code had them. `_sigtramp` (disassembled
//! with `otool -tV`; compact unwind `MODE_FRAME`) runs `stp x22, x21, [sp, #-0x30]!`,
//! `stp x29, x30, [sp, #0x20]`, `add x29, sp, #0x20`, then calls the handler with
//! `blraaz x8`. So in the `_sigtramp` frame the signal frame starts at `fp + 16`, and its
//! frame record holds the interrupted `fp` and the interrupted `x30`: following that
//! record gives the interrupted function's own caller (or a stale link register), and the
//! interrupted function is skipped. (Offsets checked with `offsetof` against the macOS
//! SDK's `<sys/ucontext.h>` and a handler printing its `ucontext` pointer on this
//! machine: `ucontext - (_sigtramp fp + 16) = 104`, `uc_mcsize = 816`.)
//!
//! The trampoline is recognized by what it leaves in memory rather than by its name, since
//! its image is usually not loaded: a caller frame (arm64 only) whose call instruction
//! (`pc - 4`) is an indirect call (`blr`, `blraa`, `blraaz`, `blrab`, `blrabz`), and whose
//! `fp + 16` holds a `ucontext` whose `uc_mcsize` is 816 and whose `uc_mcontext` points just
//! past it, with an interrupted `sp` above the signal frame or, for a handler on an
//! alternate signal stack (`sigaltstack` and `SA_ONSTACK`), below `_sigtramp`'s `sp`. Its
//! caller is then the
//! interrupted frame, with every register restored from `__ss` (all known, `x30`
//! included), and it is treated like an innermost frame: its pc is the interrupted
//! instruction, not a return address, so it is looked up at `pc` rather than `pc - 4`,
//! and the innermost-frame rules (link register, first instruction, frameless leaf) apply
//! to it. Its status is [`FrameStatus::Complete`]: the whole register state is known.
//!
//! On an alternate signal stack the kernel builds the signal frame there instead, so the
//! handler and `_sigtramp` frames are on one stack and the interrupted frame on another,
//! possibly at lower addresses. That step alone is exempt from the rule that `sp` never
//! moves down (see [Termination](#termination)): the interrupted `sp` must instead be
//! non-zero, 16-byte aligned and outside the frames from `_sigtramp`'s `sp` to the end of
//! the signal frame, and the interrupted pc executable. Every later step is checked
//! against the interrupted `sp` again.
//!
//! ## Termination
//!
//! The walk stops cleanly, without an error, when the return address is zero, when no
//! source applies, when the frame-pointer chain ends (`fp == 0`) or is not strictly
//! increasing, when the stack pointer would move down (except into the frame a signal
//! on an alternate signal stack interrupted), when memory cannot be read, or when
//! the next pc is not in executable memory ([`MemoryAccess::is_executable`]). This is how
//! the thread base (`thread_start`, dyld's `start`) ends a trace.
//!
//! ## Pointer authentication
//!
//! Every recovered return address (from memory or from the link register) goes through
//! [`Arch::strip_pointer_auth`].
//!
//! ## Caller frames
//!
//! A caller frame's pc is the return address, the instruction after the call. Its function
//! and source line are looked up at the call instruction (`pc - 4` on arm64), and its
//! unwind row too, so the reported line is the call site and a call that is the last
//! instruction of a function still finds that function.
//!
//! ## References
//!
//! - [DWARF 5, section 6.4 Call Frame Information](https://dwarfstd.org/doc/DWARF5.pdf)
//! - [gimli crate documentation](https://docs.rs/gimli/latest/gimli/)
//! - [xnu `bsd/dev/arm/unix_signal.c`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/dev/arm/unix_signal.c)
//!   (`sendsig`, `struct user_sigframe64`)

use gimli::{
    BaseAddresses, CfaRule, DebugFrame, EhFrame, EhFrameHdr, EhFrameOffset, EndianSlice, Register, RegisterRule,
    RunTimeEndian, UnwindContext, UnwindSection,
};

use crate::arch::Arch;
use crate::arch::arm64_frame::{self, FrameRegisters, Prologue};
use crate::error::Result;
use crate::symbols::compact_unwind::{self, Arm64Mode};
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
}

/// Register state of one physical frame while walking.
#[derive(Clone)]
struct Cursor
{
    regs: Registers,
    /// Bit `n` set: general register `xn` (n < 31) holds this frame's value. All are
    /// known in the innermost frame; in a caller only the callee-saved registers
    /// (`x19`-`x29`) and those its callee's CFI restored are. `x30` known means it still
    /// holds this frame's return address, which is only true before a prologue saved it.
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

/// `x0`-`x30`.
const ALL_GENERAL: u32 = (1 << 31) - 1;
/// Callee-saved registers under the arm64 Apple ABI: `x19`-`x29` (`x29` is the frame
/// pointer). Everything else is clobbered by a call, so a caller's value is unknown unless
/// CFI says where it was saved.
const CALLEE_SAVED: u32 = ((1 << 30) - 1) & !((1 << 19) - 1);
/// `x30`, the link register.
const LINK_REGISTER: u32 = 1 << 30;

/// One unwind step: the caller's registers and how they were found.
struct UnwindStep
{
    next: Registers,
    known: u32,
    status: FrameStatus,
}

/// What an image's `__unwind_info` says about the function containing an address.
enum Compact
{
    /// No `__unwind_info`, no entry, or encoding 0.
    Absent,
    /// The function's mode, whether the address is its first instruction, and the
    /// runtime address of the entry's first function.
    Mode
    {
        mode: Arm64Mode,
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
    pub fn new(arch: &'a dyn Arch, symbols: &'a SymbolCache, memory: &'a M) -> Self
    {
        Self { arch, symbols, memory }
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
        let mut frames = Vec::new();
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

            let Some(step) = next else {
                break;
            };
            status = step.status;
            let mut regs = step.next;
            // Unknown registers read as zero rather than as a callee's stale value.
            for (index, slot) in regs.general.iter_mut().enumerate().take(31) {
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

        Ok(frames)
    }

    /// The address used for symbolication and unwind lookup: the call instruction for a
    /// caller frame, the pc itself for the innermost frame.
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
        match compact_entry(image, lookup) {
            Compact::Mode {
                mode: Arm64Mode::Dwarf { fde_offset },
                ..
            } => self
                .eh_frame_fde_step(image, cursor, lookup, fde_offset)
                .or_else(|| self.frame_pointer_step(cursor)),
            Compact::Mode {
                mode: Arm64Mode::Frameless { stack_size },
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
            Compact::Mode {
                mode: Arm64Mode::Frame,
                at_entry,
                start,
            } => {
                // The linker gives one compact entry to a run of adjacent functions with
                // the same encoding, so only the first of them is recognized from the
                // entry; the others are recognized from the DWARF function index.
                if at_entry || self.is_dwarf_function_start(image, cursor) {
                    return self.entry_step(cursor);
                }
                match self.code_step(cursor, self.function_start(image, cursor, start)) {
                    Some(CodeStep::Caller(step)) => Some(step),
                    _ => self.frame_pointer_step(cursor),
                }
            }
            Compact::Mode {
                mode: Arm64Mode::None, ..
            }
            | Compact::Absent => self
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

    /// `UNWIND_ARM64_MODE_DWARF`: evaluate the FDE at `fde_offset` in `__eh_frame`
    /// directly, with no search.
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

    /// Without a compact entry: `.eh_frame_hdr` (ELF), then a linear scan of `__eh_frame`,
    /// then `.debug_frame`.
    ///
    /// `__eh_frame` FDE ranges are runtime addresses (pc-relative, resolved through the
    /// section's runtime base); `.debug_frame` lives in the unslid dSYM, so its ranges are
    /// file addresses. Each section is searched with its own address form only: trying the
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
        let mut known = cursor.known & CALLEE_SAVED;
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
    /// For the return-address column, `Undefined` and `SameValue` both mean "still in
    /// `x30`": Apple's CIEs leave that column undefined in the initial row, so before the
    /// prologue saves it the return address is the link register, if this frame's `x30`
    /// is known (the innermost frame). For any other register `Undefined` means unknown.
    fn recover(&self, cursor: &Cursor, rule: &RegisterRule<usize>, cfa: u64, register: Register) -> Option<Option<u64>>
    {
        match rule {
            RegisterRule::Undefined | RegisterRule::SameValue if register == return_register(self.arch.architecture()) => {
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

    /// `UNWIND_ARM64_MODE_FRAMELESS`: a leaf that never saves `x30` or `fp`. In the
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
            known: cursor.known & CALLEE_SAVED,
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
            known: cursor.known & CALLEE_SAVED,
            status: FrameStatus::CfiFallback,
        })
    }

    /// [`entry_step`](Self::entry_step) when the image has no compact entry: the entry
    /// is recognized from the DWARF function index.
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
    /// compact entry's first function (`entry_start`) and the nearest DWARF function start
    /// at or below the pc, since the linker merges adjacent functions into one entry.
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
        let mut x = [0; 31];
        for (slot, value) in x.iter_mut().zip(&regs.general) {
            *slot = *value;
        }
        x[29] = regs.fp.value();
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
        self.arch.set_fp(&mut next, Address::new(caller.x[29]));
        next.sp = Address::new(caller.sp);
        next.pc = self.arch.strip_pointer_auth(Address::new(caller.x[30]));
        Some(CodeStep::Caller(UnwindStep {
            next,
            known: caller.known & CALLEE_SAVED,
            status: FrameStatus::CfiFallback,
        }))
    }

    /// Follow the frame record at `fp`: `[fp]` is the caller's `fp`, `[fp + 8]` the return
    /// address.
    fn frame_pointer_step(&self, cursor: &Cursor) -> Option<UnwindStep>
    {
        let regs = &cursor.regs;
        if cursor.known & (1 << 29) == 0 {
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
            known: 1 << 29,
            status: FrameStatus::CfiFallback,
        })
    }

    /// A caller frame in `_sigtramp`: the frame the signal interrupted, with every register
    /// restored from the signal frame's `__darwin_mcontext64` (see
    /// [Signal handler frames](self#signal-handler-frames)). `None` for any other frame.
    fn signal_frame_step(&self, cursor: &Cursor) -> Option<UnwindStep>
    {
        if !cursor.is_caller || self.arch.architecture() != Architecture::Arm64 || cursor.known & (1 << 29) == 0 {
            return None;
        }
        // Little-endian: the low half of the word at `pc - 4` is the call instruction.
        let call = self
            .memory
            .read_u64(Address::new(cursor.regs.pc.value().checked_sub(4)?))
            .ok()?;
        if !is_indirect_call(call as u32) {
            return None;
        }
        let frame = cursor.regs.fp.value().checked_add(16)?;
        let ucontext = frame.checked_add(sigframe::UCONTEXT)?;
        let mcontext = frame.checked_add(sigframe::MCONTEXT)?;
        let read = |offset: u64| {
            let address = mcontext.checked_add(offset)?;
            self.memory.read_u64(Address::new(address)).ok()
        };
        let mcsize = self
            .memory
            .read_u64(Address::new(ucontext.checked_add(sigframe::UC_MCSIZE)?))
            .ok()?;
        let mcontext_pointer = self
            .memory
            .read_u64(Address::new(ucontext.checked_add(sigframe::UC_MCONTEXT)?))
            .ok()?;
        if mcsize != sigframe::MCONTEXT_SIZE || mcontext_pointer != mcontext {
            return None;
        }
        let sp = read(sigframe::SS_SP)?;
        // On the interrupted stack the signal frame lies below the interrupted `sp`. On an
        // alternate signal stack the interrupted `sp` is on another stack, and may be below
        // the handler's frames; it can never be among them, from `_sigtramp`'s `sp` to the
        // end of the signal frame.
        let on_alternate_stack = if sp >= mcontext.checked_add(sigframe::MCONTEXT_SIZE)? {
            false
        } else if sp < cursor.regs.sp.value() {
            true
        } else {
            return None;
        };

        let mut next = cursor.regs.clone();
        for (index, slot) in next.general.iter_mut().enumerate().take(29) {
            *slot = read(sigframe::SS_X + 8 * index as u64)?;
        }
        self.arch.set_fp(&mut next, Address::new(read(sigframe::SS_FP)?));
        let lr = read(sigframe::SS_LR)?;
        if let Some(slot) = next.general.get_mut(30) {
            *slot = lr;
        }
        next.sp = Address::new(sp);
        next.pc = self.arch.strip_pointer_auth(Address::new(read(sigframe::SS_PC)?));
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
    /// pointer down: from `_sigtramp` on an alternate signal stack to the frame the signal
    /// interrupted. The interrupted state comes from a validated `ucontext`, so instead of
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
        match register.0 {
            29 => self.arch.set_fp(regs, Address::new(value)),
            31 => regs.sp = Address::new(value),
            n @ 0..=30 => {
                if let Some(slot) = regs.general.get_mut(usize::from(n)) {
                    *slot = value;
                }
            }
            _ => {}
        }
    }
}

/// Offsets in xnu's arm64 `struct user_sigframe64` (see
/// [Signal handler frames](self#signal-handler-frames)).
mod sigframe
{
    /// The `ucontext_t`, after the 104-byte `siginfo_t`.
    pub(super) const UCONTEXT: u64 = 104;
    /// `uc_mcsize` in `ucontext_t`.
    pub(super) const UC_MCSIZE: u64 = 40;
    /// `uc_mcontext` (a pointer) in `ucontext_t`.
    pub(super) const UC_MCONTEXT: u64 = 48;
    /// The `__darwin_mcontext64`, after the 56-byte `ucontext_t`.
    pub(super) const MCONTEXT: u64 = UCONTEXT + 56;
    /// `sizeof(__darwin_mcontext64)`, the kernel's `uc_mcsize` (`UC_FLAVOR_SIZE64`).
    pub(super) const MCONTEXT_SIZE: u64 = 816;
    /// `__ss.__x[0]` in the mcontext, after the 16-byte `__es`.
    pub(super) const SS_X: u64 = 16;
    /// `__ss.__fp`.
    pub(super) const SS_FP: u64 = SS_X + 232;
    /// `__ss.__lr`.
    pub(super) const SS_LR: u64 = SS_X + 240;
    /// `__ss.__sp`.
    pub(super) const SS_SP: u64 = SS_X + 248;
    /// `__ss.__pc`.
    pub(super) const SS_PC: u64 = SS_X + 256;
}

/// Whether an arm64 instruction is a register-indirect call: `blr`, or the pointer
/// authenticating `blraa`, `blraaz`, `blrab`, `blrabz`.
fn is_indirect_call(instruction: u32) -> bool
{
    instruction & 0xffff_fc1f == 0xd63f_0000 || instruction & 0xfeff_f800 == 0xd63f_0800
}

/// The `known` bit of an arm64 DWARF register number `x0`-`x30`.
fn general_bit(register: Register) -> Option<u32>
{
    (register.0 <= 30).then(|| 1 << register.0)
}

/// The compact-unwind entry of the function containing `lookup`, if the image has one.
fn compact_entry(image: &BinaryImage, lookup: Address) -> Compact
{
    let Some(section) = image.unwind_info_section() else {
        return Compact::Absent;
    };
    let (header, _) = image.runtime_range();
    let Some(offset) = lookup
        .value()
        .checked_sub(header)
        .and_then(|offset| u32::try_from(offset).ok())
    else {
        return Compact::Absent;
    };
    match compact_unwind::lookup(section, offset) {
        Some(entry) if entry.encoding != 0 => Compact::Mode {
            mode: Arm64Mode::decode(entry.encoding),
            at_entry: entry.function_offset == offset,
            start: Address::new(header + u64::from(entry.function_offset)),
        },
        _ => Compact::Absent,
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
    let reg_num = register.0;
    match architecture {
        Architecture::Arm64 => match reg_num {
            31 => Some(regs.sp.value()),
            29 => Some(regs.fp.value()),
            0..=30 => regs.general.get(usize::from(reg_num)).copied(),
            _ => None,
        },
        Architecture::X86_64 => match reg_num {
            7 => Some(regs.sp.value()),
            16 => Some(regs.pc.value()),
            6 => Some(regs.fp.value()),
            0..=15 => {
                let mapping = [0, 2, 1, 3, 6, 7, 5, 4, 8, 9, 10, 11, 12, 13, 14, 15];
                mapping
                    .get(usize::from(reg_num))
                    .and_then(|&idx| regs.general.get(idx).copied())
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
        _ => Register(30),
    }
}

#[cfg(test)]
mod tests
{
    use std::collections::HashMap;

    use super::*;
    use crate::arch::Arm64;
    use crate::backend::mock::arm64_registers;
    use crate::error::DebuggerError;

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
        let symbols = SymbolCache::new();
        StackUnwinder::new(&ARCH, &symbols, memory)
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
        let symbols = SymbolCache::new();
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory);
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
        let symbols = SymbolCache::new();
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory);
        let cursor = Cursor {
            known: CALLEE_SAVED,
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

    /// `blraaz x8`, the call in `_sigtramp`, and `bl` (a direct call).
    const BLRAAZ_X8: u64 = 0xd63f_091f;
    const BL: u64 = 0x9400_0010;

    /// A handler frame record at 0x1000 returning into a `_sigtramp` at 0x5000_0040 whose
    /// frame record (0x1100) sits right below an xnu signal frame; the interrupted frame
    /// is at pc 0x4000_0100 with sp 0x2000, fp 0x2100, lr 0x4000_0aa0, and its own frame
    /// record at 0x2100 returns to 0x4000_0200 (fp chain end).
    fn signal_stack() -> FakeMemory
    {
        let mut memory = FakeMemory::default();
        memory.words.insert(0x1000, 0x1100);
        memory.words.insert(0x1008, 0x5000_0044);
        // `_sigtramp`'s frame record: the interrupted fp and x30, not a caller of it.
        memory.words.insert(0x1100, 0x2100);
        memory.words.insert(0x1108, 0x4000_0aa0);
        memory.words.insert(0x5000_0040, BLRAAZ_X8);
        let frame = 0x1110;
        let ucontext = frame + sigframe::UCONTEXT;
        let mcontext = frame + sigframe::MCONTEXT;
        memory.words.insert(ucontext + sigframe::UC_MCSIZE, sigframe::MCONTEXT_SIZE);
        memory.words.insert(ucontext + sigframe::UC_MCONTEXT, mcontext);
        for index in 0..29 {
            memory.words.insert(mcontext + sigframe::SS_X + 8 * index, 0x100 + index);
        }
        memory.words.insert(mcontext + sigframe::SS_FP, 0x2100);
        memory.words.insert(mcontext + sigframe::SS_LR, 0x4000_0aa0);
        memory.words.insert(mcontext + sigframe::SS_SP, 0x2000);
        memory.words.insert(mcontext + sigframe::SS_PC, 0x8012_0000_4000_0100);
        memory.words.insert(0x2100, 0);
        memory.words.insert(0x2108, 0x4000_0200);
        memory
    }

    #[test]
    fn a_signal_frame_restores_the_interrupted_frame()
    {
        let memory = signal_stack();
        let symbols = SymbolCache::new();
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
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
        let symbols = SymbolCache::new();
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory);
        let sigtramp = Cursor {
            known: 1 << 29,
            is_caller: true,
            ..innermost(regs(0x5000_0044, 0x10f0, 0x1100, 0))
        };
        let step = unwinder.signal_frame_step(&sigtramp).expect("a signal frame");
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
    fn a_frame_without_a_signal_frame_is_not_a_trampoline()
    {
        let symbols = SymbolCache::new();
        let sigtramp = Cursor {
            known: 1 << 29,
            is_caller: true,
            ..innermost(regs(0x5000_0044, 0x10f0, 0x1100, 0))
        };
        // A direct call before the return address.
        let mut memory = signal_stack();
        memory.words.insert(0x5000_0040, BL);
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .signal_frame_step(&sigtramp)
                .is_none()
        );
        // A `uc_mcontext` that does not point just past the `ucontext`.
        let mut memory = signal_stack();
        memory
            .words
            .insert(0x1110 + sigframe::UCONTEXT + sigframe::UC_MCONTEXT, 0x9999);
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .signal_frame_step(&sigtramp)
                .is_none()
        );
        // The innermost frame is never a trampoline's caller frame.
        let memory = signal_stack();
        let innermost = Cursor {
            is_caller: false,
            ..sigtramp
        };
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .signal_frame_step(&innermost)
                .is_none()
        );
    }

    /// [`signal_stack`] with the signal frame on an alternate signal stack at 0x9000:
    /// the handler's frame record at 0x9000 returns into `_sigtramp`, whose record at
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
        let mcontext = 0x9110 + sigframe::MCONTEXT;
        memory
            .words
            .insert(0x9110 + sigframe::UCONTEXT + sigframe::UC_MCONTEXT, mcontext);
        memory
    }

    fn alternate_stack_sigtramp() -> Cursor
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
        let symbols = SymbolCache::new();
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
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
        let symbols = SymbolCache::new();
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0x8f00, 0x9000, 0), 64)
            .expect("unwind");
        let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
        assert_eq!(pcs, [0x4000_0000, 0x5000_0044, 0x4000_0100]);
    }

    #[test]
    fn an_alternate_stack_signal_frame_still_needs_a_plausible_interrupted_state()
    {
        let symbols = SymbolCache::new();
        let mcontext = 0x9110 + sigframe::MCONTEXT;
        let cursor = alternate_stack_sigtramp();
        assert!(
            StackUnwinder::new(&ARCH, &symbols, &alternate_signal_stack())
                .signal_frame_step(&cursor)
                .is_some()
        );
        let rejected = |change: &dyn Fn(&mut FakeMemory)| {
            let mut memory = alternate_signal_stack();
            change(&mut memory);
            StackUnwinder::new(&ARCH, &symbols, &memory)
                .signal_frame_step(&cursor)
                .is_none()
        };
        // An interrupted sp among the handler's frames on the alternate stack.
        assert!(rejected(&|memory| {
            memory.words.insert(mcontext + sigframe::SS_SP, 0x9200);
        }));
        // A zero or misaligned interrupted sp.
        assert!(rejected(&|memory| {
            memory.words.insert(mcontext + sigframe::SS_SP, 0);
        }));
        assert!(rejected(&|memory| {
            memory.words.insert(mcontext + sigframe::SS_SP, 0x2008);
        }));
        // An interrupted pc outside code.
        assert!(rejected(&|memory| {
            memory.code = Some((0x4000_0000, 0x4000_0100));
        }));
        // An unreadable mcontext.
        assert!(rejected(&|memory| {
            memory.words.remove(&(mcontext + sigframe::SS_X + 8 * 7));
        }));
        // A wrong `uc_mcsize`.
        assert!(rejected(&|memory| {
            memory.words.insert(0x9110 + sigframe::UCONTEXT + sigframe::UC_MCSIZE, 512);
        }));
    }

    #[test]
    fn a_corrupt_alternate_stack_context_cannot_loop_the_trace()
    {
        // The saved context points back at the handler's own frame on the alternate
        // stack, so taking it again and again would repeat the same frames until
        // `max_frames`. Only the first downward signal step is taken.
        let mut memory = alternate_signal_stack();
        let mcontext = 0x9110 + sigframe::MCONTEXT;
        memory.words.insert(mcontext + sigframe::SS_SP, 0x8f00);
        memory.words.insert(mcontext + sigframe::SS_FP, 0x9000);
        memory.words.insert(mcontext + sigframe::SS_PC, 0x4000_0000);
        let trace = pcs(&memory, &regs(0x4000_0000, 0x8f00, 0x9000, 0));
        assert!(trace.len() < 8, "the trace loops: {trace:x?}");
        assert_eq!(
            trace.iter().filter(|&&pc| pc == 0x5000_0044).count(),
            2,
            "`_sigtramp` is reached at most twice: {trace:x?}"
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
        let symbols = SymbolCache::new();
        let memory = alternate_signal_stack();
        let unwinder = StackUnwinder::new(&ARCH, &symbols, &memory);
        let caller = alternate_stack_sigtramp();
        let signal = unwinder.signal_frame_step(&caller).expect("the alternate-stack signal step");
        assert_eq!(signal.next.sp, Address::new(0x2000));
        assert!(
            unwinder.accept(&caller, signal).is_none(),
            "accept keeps sp monotonic for every other step"
        );
    }

    #[test]
    fn indirect_calls_are_recognized()
    {
        // blr x8, blraa x8, x9, blraaz x8, blrab x8, x9, blrabz x8.
        for call in [0xd63f_0100, 0xd73f_0909, 0xd63f_091f, 0xd73f_0d09, 0xd63f_0d1f] {
            assert!(is_indirect_call(call), "{call:#x}");
        }
        // bl, br x8, ret.
        for other in [0x9400_0010, 0xd61f_0100, 0xd65f_03c0] {
            assert!(!is_indirect_call(other), "{other:#x}");
        }
    }

    #[test]
    fn max_frames_limits_the_trace()
    {
        let memory = chain();
        let symbols = SymbolCache::new();
        let frames = StackUnwinder::new(&ARCH, &symbols, &memory)
            .unwind(ThreadId::from(1), &regs(0x4000_0000, 0xf00, 0x1000, 0), 2)
            .expect("unwind");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].index, 1);
        assert_eq!(frames[0].return_address, Some(Address::new(0x4000_0010)));
    }
}
