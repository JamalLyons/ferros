//! # The macOS runtime: non-local exit entries and the `_sigtramp` signal frame
//!
//! [`MacOSRuntime`] implements [`ferros_core::runtime::TargetRuntime`] for macOS
//! processes.
//!
//! ## Non-local exits
//!
//! Two routes are trapped: the system unwinder resuming at a landing pad, and `_longjmp`.
//!
//! - `libunwind.dylib`: `unw_resume` and the variant `_Unwind_RaiseException` and
//!   `_Unwind_Resume` call on current macOS, `unw_resume_with_frames_walked`. Matched with
//!   every leading underscore removed: the shared cache's symbol table names them
//!   `___unw_resume` (the internal alias), the exports `_unw_resume`.
//! - `libsystem_platform.dylib`: `__longjmp` (the C function `_longjmp`, the leaf that
//!   restores a `jmp_buf`). `longjmp` (raw `_longjmp`) and `siglongjmp` restore the signal
//!   state and branch to it, so trapping it covers all three without stepping through
//!   their system calls.
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
//! with `otool -tV`; a frame-record function) runs `stp x22, x21, [sp, #-0x30]!`,
//! `stp x29, x30, [sp, #0x20]`, `add x29, sp, #0x20`, then calls the handler with
//! `blraaz x8`. So in the `_sigtramp` frame the signal frame starts at `fp + 16`, and its
//! frame record holds the interrupted `fp` and the interrupted `x30`. (Offsets checked
//! with `offsetof` against the macOS SDK's `<sys/ucontext.h>` and a handler printing its
//! `ucontext` pointer on this machine: `ucontext - (_sigtramp fp + 16) = 104`,
//! `uc_mcsize = 816`.)
//!
//! The trampoline is recognized by what it leaves in memory rather than by its name, since
//! its image is usually not loaded: a caller frame (arm64 only) with a known frame
//! pointer whose call instruction (`pc - 4`) is an indirect call (`blr`, `blraa`,
//! `blraaz`, `blrab`, `blrabz`), and whose `fp + 16` holds a `ucontext` whose `uc_mcsize`
//! is 816 and whose `uc_mcontext` points just past it. The engine then checks the
//! interrupted `sp` against the end of the signal frame (see
//! `ferros_core::symbols::unwind`).
//!
//! ## References
//!
//! - [xnu `bsd/dev/arm/unix_signal.c`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/dev/arm/unix_signal.c)
//!   (`sendsig`, `struct user_sigframe64`)

use std::sync::{Arc, LazyLock};

use ferros_core::arch::Arm64;
use ferros_core::runtime::{ExitSource, SignalContext, TargetRuntime, TrampolineFrame};
use ferros_core::symbols::unwind::MemoryAccess;
use ferros_core::types::{Address, Architecture};

/// Functions through which the unwinder transfers control to a landing pad, without
/// their leading underscores.
const RESUME_SYMBOLS: [&str; 2] = ["unw_resume", "unw_resume_with_frames_walked"];

/// The image holding the unwinder's resume functions.
pub const UNWINDER_IMAGE: &str = "libunwind.dylib";
/// The image holding `__longjmp` and `_sigtramp`.
pub const PLATFORM_IMAGE: &str = "libsystem_platform.dylib";

/// The macOS runtime of the [module documentation](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct MacOSRuntime;

impl MacOSRuntime
{
    /// The runtime as a session holds it.
    #[must_use]
    pub fn shared() -> Arc<dyn TargetRuntime>
    {
        Arc::new(Self)
    }
}

fn is_unwinder_resume(raw: &str) -> bool
{
    RESUME_SYMBOLS.contains(&raw.trim_start_matches('_'))
}

fn is_long_jump(raw: &str) -> bool
{
    raw == "__longjmp"
}

static EXIT_SOURCES: LazyLock<[ExitSource; 2]> = LazyLock::new(|| {
    [
        ExitSource::new(UNWINDER_IMAGE, is_unwinder_resume),
        ExitSource::new(PLATFORM_IMAGE, is_long_jump),
    ]
});

impl TargetRuntime for MacOSRuntime
{
    fn exit_sources(&self) -> &[ExitSource]
    {
        &*EXIT_SOURCES
    }

    fn signal_context(&self, memory: &dyn MemoryAccess, frame: &TrampolineFrame) -> Option<SignalContext>
    {
        if frame.architecture != Architecture::Arm64 {
            return None;
        }
        let fp = frame.fp?;
        // Little-endian: the low half of the word at `pc - 4` is the call instruction.
        let call = memory.read_u64(Address::new(frame.pc.value().checked_sub(4)?)).ok()?;
        if !Arm64::is_indirect_call(call as u32) {
            return None;
        }
        let start = fp.value().checked_add(16)?;
        let ucontext = start.checked_add(sigframe::UCONTEXT)?;
        let mcontext = start.checked_add(sigframe::MCONTEXT)?;
        let at = |base: u64, offset: u64| -> Option<u64> { memory.read_u64(Address::new(base.checked_add(offset)?)).ok() };
        if at(ucontext, sigframe::UC_MCSIZE)? != sigframe::MCONTEXT_SIZE || at(ucontext, sigframe::UC_MCONTEXT)? != mcontext
        {
            return None;
        }
        let mut general = Vec::with_capacity(29);
        for index in 0..29 {
            general.push(at(mcontext, sigframe::SS_X + 8 * index)?);
        }
        Some(SignalContext {
            general,
            fp: at(mcontext, sigframe::SS_FP)?,
            lr: at(mcontext, sigframe::SS_LR)?,
            sp: at(mcontext, sigframe::SS_SP)?,
            pc: at(mcontext, sigframe::SS_PC)?,
            frame_end: Address::new(mcontext.checked_add(sigframe::MCONTEXT_SIZE)?),
        })
    }
}

/// Offsets in xnu's arm64 `struct user_sigframe64` (see
/// [Signal handler frames](self#signal-handler-frames)).
pub mod sigframe
{
    /// The `ucontext_t`, after the 104-byte `siginfo_t`.
    pub const UCONTEXT: u64 = 104;
    /// `uc_mcsize` in `ucontext_t`.
    pub const UC_MCSIZE: u64 = 40;
    /// `uc_mcontext` (a pointer) in `ucontext_t`.
    pub const UC_MCONTEXT: u64 = 48;
    /// The `__darwin_mcontext64`, after the 56-byte `ucontext_t`.
    pub const MCONTEXT: u64 = UCONTEXT + 56;
    /// `sizeof(__darwin_mcontext64)`, the kernel's `uc_mcsize` (`UC_FLAVOR_SIZE64`).
    pub const MCONTEXT_SIZE: u64 = 816;
    /// `__ss.__x[0]` in the mcontext, after the 16-byte `__es`.
    pub const SS_X: u64 = 16;
    /// `__ss.__fp`.
    pub const SS_FP: u64 = SS_X + 232;
    /// `__ss.__lr`.
    pub const SS_LR: u64 = SS_X + 240;
    /// `__ss.__sp`.
    pub const SS_SP: u64 = SS_X + 248;
    /// `__ss.__pc`.
    pub const SS_PC: u64 = SS_X + 256;
}

#[cfg(test)]
mod tests
{
    use std::collections::HashMap;

    use ferros_core::error::{DebuggerError, Result};

    use super::*;

    /// Little-endian words at addresses; anything else is unreadable.
    #[derive(Default)]
    struct FakeMemory
    {
        words: HashMap<u64, u64>,
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
    }

    /// `blraaz x8`, the call in `_sigtramp`, and `bl` (a direct call).
    const BLRAAZ_X8: u64 = 0xd63f_091f;
    const BL: u64 = 0x9400_0010;
    /// `_sigtramp`'s frame pointer; its signal frame starts 16 bytes above.
    const FP: u64 = 0x1100;
    const MCONTEXT: u64 = FP + 16 + sigframe::MCONTEXT;

    /// A `_sigtramp` frame returning to 0x5000_0044 after `blraaz x8`, its xnu signal
    /// frame at `FP + 16`: interrupted `x0`-`x28` are `0x100 + n`, fp 0x2100, lr
    /// 0x4000_0aa0, sp 0x2000, pc 0x4000_0100 with pointer-authentication bits.
    fn signal_frame() -> FakeMemory
    {
        let mut memory = FakeMemory::default();
        memory.words.insert(0x5000_0040, BLRAAZ_X8);
        let ucontext = FP + 16 + sigframe::UCONTEXT;
        memory.words.insert(ucontext + sigframe::UC_MCSIZE, sigframe::MCONTEXT_SIZE);
        memory.words.insert(ucontext + sigframe::UC_MCONTEXT, MCONTEXT);
        for index in 0..29 {
            memory.words.insert(MCONTEXT + sigframe::SS_X + 8 * index, 0x100 + index);
        }
        memory.words.insert(MCONTEXT + sigframe::SS_FP, 0x2100);
        memory.words.insert(MCONTEXT + sigframe::SS_LR, 0x4000_0aa0);
        memory.words.insert(MCONTEXT + sigframe::SS_SP, 0x2000);
        memory.words.insert(MCONTEXT + sigframe::SS_PC, 0x8012_0000_4000_0100);
        memory
    }

    fn trampoline() -> TrampolineFrame
    {
        TrampolineFrame {
            architecture: Architecture::Arm64,
            pc: Address::new(0x5000_0044),
            sp: Address::new(0x10f0),
            fp: Some(Address::new(FP)),
        }
    }

    #[test]
    fn a_sigtramp_frame_yields_the_saved_thread_state()
    {
        let context = MacOSRuntime
            .signal_context(&signal_frame(), &trampoline())
            .expect("a signal frame");
        assert_eq!(context.general.len(), 29);
        assert_eq!(context.general[0], 0x100);
        assert_eq!(context.general[28], 0x100 + 28);
        assert_eq!(context.fp, 0x2100);
        assert_eq!(context.lr, 0x4000_0aa0);
        assert_eq!(context.sp, 0x2000);
        assert_eq!(context.pc, 0x8012_0000_4000_0100, "the raw pc, stripped by the engine");
        assert_eq!(context.frame_end, Address::new(MCONTEXT + sigframe::MCONTEXT_SIZE));
    }

    #[test]
    fn a_frame_without_a_signal_frame_is_not_a_trampoline()
    {
        let none = |memory: &FakeMemory, frame: &TrampolineFrame| MacOSRuntime.signal_context(memory, frame).is_none();
        // A direct call before the return address.
        let mut memory = signal_frame();
        memory.words.insert(0x5000_0040, BL);
        assert!(none(&memory, &trampoline()));
        // A `uc_mcontext` that does not point just past the `ucontext`.
        let mut memory = signal_frame();
        memory
            .words
            .insert(FP + 16 + sigframe::UCONTEXT + sigframe::UC_MCONTEXT, 0x9999);
        assert!(none(&memory, &trampoline()));
        // A wrong `uc_mcsize`.
        let mut memory = signal_frame();
        memory.words.insert(FP + 16 + sigframe::UCONTEXT + sigframe::UC_MCSIZE, 512);
        assert!(none(&memory, &trampoline()));
        // An unreadable register.
        let mut memory = signal_frame();
        memory.words.remove(&(MCONTEXT + sigframe::SS_X + 8 * 7));
        assert!(none(&memory, &trampoline()));
        // An unknown frame pointer, or another architecture.
        let memory = signal_frame();
        assert!(none(
            &memory,
            &TrampolineFrame {
                fp: None,
                ..trampoline()
            }
        ));
        assert!(none(
            &memory,
            &TrampolineFrame {
                architecture: Architecture::X86_64,
                ..trampoline()
            }
        ));
    }

    #[test]
    fn exit_entries_are_the_unwinder_resumes_and_long_jump()
    {
        let sources = MacOSRuntime.exit_sources();
        let unwinder = sources
            .iter()
            .find(|source| source.image() == UNWINDER_IMAGE)
            .expect("libunwind");
        for name in ["_unw_resume", "___unw_resume", "_unw_resume_with_frames_walked"] {
            assert!(unwinder.is_entry(name), "{name}");
        }
        assert!(!unwinder.is_entry("_unw_step"));
        let platform = sources
            .iter()
            .find(|source| source.image() == PLATFORM_IMAGE)
            .expect("platform");
        assert!(platform.is_entry("__longjmp"));
        for name in ["_longjmp", "_siglongjmp", "_longjmperror"] {
            assert!(!platform.is_entry(name), "{name}");
        }
    }
}
