//! Stack traces and symbolication.

use std::cell::{OnceCell, RefCell};

use super::Session;
use crate::backend::Backend;
use crate::error::{DebuggerError, Result};
use crate::symbols::Symbolication;
use crate::symbols::unwind::{MemoryAccess, StackUnwinder};
use crate::types::{Address, MemoryRegion, StackFrame};

/// [`MemoryAccess`] over a backend.
///
/// The unwinder reads through `&self` while [`Backend::read_memory`] needs `&mut`, so the
/// borrow is kept in a `RefCell`. The unwinder never reads re-entrantly, so the borrow
/// never conflicts.
///
/// Executable regions are listed once per trace, on the first question, so a trace that
/// never needs them (a single frame) does not ask the backend. A backend that cannot list
/// regions answers every address as executable.
struct BackendMemory<'a>
{
    backend: RefCell<&'a mut dyn Backend>,
    executable: OnceCell<Option<Vec<MemoryRegion>>>,
}

impl<'a> BackendMemory<'a>
{
    fn new(backend: &'a mut dyn Backend) -> Self
    {
        Self {
            backend: RefCell::new(backend),
            executable: OnceCell::new(),
        }
    }
}

impl MemoryAccess for BackendMemory<'_>
{
    fn read_u64(&self, address: Address) -> Result<u64>
    {
        let bytes = self.backend.borrow_mut().read_memory(address, 8)?;
        let bytes: [u8; 8] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            DebuggerError::InvalidArgument(format!(
                "read {} bytes instead of 8 at 0x{:016x}",
                bytes.len(),
                address.value()
            ))
        })?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn is_executable(&self, address: Address) -> bool
    {
        let regions = self.executable.get_or_init(|| {
            let regions = self.backend.borrow_mut().memory_regions().ok()?;
            Some(
                regions
                    .into_iter()
                    .filter(|region| region.permissions.contains('x'))
                    .collect(),
            )
        });
        regions
            .as_ref()
            .is_none_or(|regions| regions.iter().any(|region| region.start <= address && address < region.end))
    }
}

impl Session
{
    /// Unwind the selected thread's stack, at most `max_frames` logical frames (inlined
    /// frames included), innermost first.
    ///
    /// The unwind sources and their order are described in
    /// [`symbols::unwind`](crate::symbols::unwind). Every frame's pc lies in executable
    /// memory as the backend reports it ([`Backend::memory_regions`]); the trace ends at
    /// the thread's base instead of guessing. Caller frames are symbolicated at their call
    /// instruction, so their line is the call site.
    ///
    /// The backend's images are loaded into the symbol cache first (see
    /// [`images`](Self::images)); frames in images that could not be loaded have no
    /// symbol. Memory is read through the backend as little-endian 64-bit words.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::NoThreadSelected`] if the process has no
    /// thread, [`DebuggerError::Backend`] with `Unsupported` for an architecture without
    /// an [`Arch`](crate::arch::Arch), or an error from the backend.
    pub fn stack_trace(&mut self, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        self.require_stopped()?;
        let thread = self.selected_or_error()?;
        let registers = self.backend.read_registers(thread)?;
        self.load_images()?;
        self.begin_symbol_request();

        // Missing symbols never prevent returning raw stack frames.
        self.load_symbols_for_frame(registers.pc);
        let arch = self.arch()?;
        // Each pass repeats only after a successful load, which marks one live image as
        // loaded. Thus this loop terminates after at most the live image count, even when
        // a caller's newly available CFI reveals further images that the frame-pointer
        // pass missed. A failed load is not retried within this request, so a bad image
        // is read at most once per trace.
        loop {
            let frames = {
                let memory = BackendMemory::new(self.backend.as_mut());
                StackUnwinder::new(arch, &self.symbols, &memory).unwind(thread, &registers, max_frames)?
            };
            let mut added = false;
            for frame in &frames {
                added |= self.load_symbols_for_frame(frame.pc);
                // A caller is symbolicated at its call instruction, which can lie in the
                // previous image when the call was the image's last instruction.
                added |= self.load_symbols_for_frame(Address::new(frame.pc.value().saturating_sub(4)));
            }
            if !added {
                return Ok(frames);
            }
        }
    }

    /// Load symbols for a frame's image. A failure leaves the frame without a symbol and
    /// is logged, since a stack trace still returns raw frames.
    fn load_symbols_for_frame(&mut self, pc: Address) -> bool
    {
        self.load_symbols_for_address(pc).unwrap_or_else(|error| {
            tracing::debug!("stack trace continues without symbols at {pc}: {error}");
            false
        })
    }

    /// Map `address` to function names and source locations.
    ///
    /// Returns `Ok(None)` when no loaded image contains the address or it has no symbol.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, the backend's enumeration error, [`DebuggerError::NoDebugInfo`]
    /// when the owning image has no usable DWARF, or [`DebuggerError::SymbolLoad`] with
    /// the original failure when the owning image's symbols could not be loaded for
    /// another reason. That failure is not cached: a later call tries again.
    pub fn symbolicate(&mut self, address: Address) -> Result<Option<Symbolication>>
    {
        self.require_stopped()?;
        self.load_images()?;
        self.begin_symbol_request();
        self.load_symbols_for_address(address)?;
        Ok(self.symbols.symbolicate(address))
    }
}
