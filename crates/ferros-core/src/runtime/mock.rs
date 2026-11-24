//! A made-up runtime for tests: exit sources chosen by the test, and a signal trampoline
//! with a simple saved-state layout.
//!
//! [`MockRuntime::with_signal_trampoline`] names the code range of a trampoline. A caller
//! frame whose pc lies in it, and whose frame pointer is known, has its saved state at
//! `fp + 16`, as little-endian `u64` words: `x0`-`x28`, then `fp`, `lr`, `sp` and `pc`
//! ([`MockRuntime::CONTEXT_WORDS`] words). The state ends right after them.

use std::ops::Range;
use std::sync::Arc;

use super::{ExitSource, SignalContext, TargetRuntime, TrampolineFrame};
use crate::symbols::unwind::MemoryAccess;
use crate::types::{Address, Architecture};

/// The made-up runtime of the [module documentation](self).
#[derive(Debug, Clone, Default)]
pub struct MockRuntime
{
    exits: Vec<ExitSource>,
    trampoline: Option<Range<u64>>,
}

impl MockRuntime
{
    /// General registers saved before `fp`, `lr`, `sp` and `pc`.
    pub const GENERAL_WORDS: u64 = 29;
    /// Words of saved state.
    pub const CONTEXT_WORDS: u64 = Self::GENERAL_WORDS + 4;
    /// Offset of the saved state from the trampoline frame's `fp`.
    pub const CONTEXT_OFFSET: u64 = 16;
    /// Offset of the saved `fp` in the state.
    pub const FP: u64 = 8 * Self::GENERAL_WORDS;
    /// Offset of the saved `lr`.
    pub const LR: u64 = Self::FP + 8;
    /// Offset of the saved `sp`.
    pub const SP: u64 = Self::FP + 16;
    /// Offset of the saved `pc`.
    pub const PC: u64 = Self::FP + 24;

    /// A runtime with no exit sources and no signal trampoline.
    #[must_use]
    pub fn new() -> Self
    {
        Self::default()
    }

    /// This runtime with `source` among its exit sources.
    #[must_use]
    pub fn with_exit_source(mut self, source: ExitSource) -> Self
    {
        self.exits.push(source);
        self
    }

    /// This runtime with a signal trampoline whose code is `code`.
    #[must_use]
    pub fn with_signal_trampoline(mut self, code: Range<u64>) -> Self
    {
        self.trampoline = Some(code);
        self
    }

    /// This runtime as a session holds it.
    #[must_use]
    pub fn into_shared(self) -> Arc<dyn TargetRuntime>
    {
        Arc::new(self)
    }
}

impl TargetRuntime for MockRuntime
{
    fn exit_sources(&self) -> &[ExitSource]
    {
        &self.exits
    }

    fn signal_context(&self, memory: &dyn MemoryAccess, frame: &TrampolineFrame) -> Option<SignalContext>
    {
        let code = self.trampoline.as_ref()?;
        if frame.architecture != Architecture::Arm64 || !code.contains(&frame.pc.value()) {
            return None;
        }
        let context = frame.fp?.value().checked_add(Self::CONTEXT_OFFSET)?;
        let read = |offset: u64| memory.read_u64(Address::new(context.checked_add(offset)?)).ok();
        let mut general = Vec::new();
        for index in 0..Self::GENERAL_WORDS {
            general.push(read(8 * index)?);
        }
        Some(SignalContext {
            general,
            fp: read(Self::FP)?,
            lr: read(Self::LR)?,
            sp: read(Self::SP)?,
            pc: read(Self::PC)?,
            frame_end: Address::new(context.checked_add(8 * Self::CONTEXT_WORDS)?),
        })
    }
}
