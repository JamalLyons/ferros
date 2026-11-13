//! Stack traces and symbolication.

use std::cell::RefCell;

use super::Session;
use crate::backend::Backend;
use crate::error::{DebuggerError, Result};
use crate::symbols::Symbolication;
use crate::symbols::unwind::{MemoryAccess, StackUnwinder};
use crate::types::{Address, StackFrame};

/// [`MemoryAccess`] over a backend.
///
/// The unwinder reads through `&self` while [`Backend::read_memory`] needs `&mut`, so the
/// borrow is kept in a `RefCell`. The unwinder never reads re-entrantly, so the borrow
/// never conflicts.
struct BackendMemory<'a>
{
    backend: RefCell<&'a mut dyn Backend>,
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
}

impl Session
{
    /// Unwind the selected thread's stack, at most `max_frames` frames, innermost first.
    ///
    /// The backend's images are loaded into the symbol cache first (see
    /// [`images`](Self::images)); frames in images that could not be loaded have no
    /// symbol. Memory is read through the backend as little-endian 64-bit words.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::NoThreadSelected`] if the process has no
    /// thread, or an error from the backend or the unwinder.
    pub fn stack_trace(&mut self, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        self.require_stopped()?;
        let thread = self.selected_or_error()?;
        let registers = self.backend.read_registers(thread)?;
        self.load_images()?;

        // Missing symbols never prevent returning raw stack frames.
        let _ = self.load_symbols_for_address(registers.pc);
        let architecture = self.backend.architecture();
        // Each successful load marks one live image as attempted. Thus this loop
        // terminates after at most the live image count, even when a caller's newly
        // available CFI reveals further images that the frame-pointer pass missed.
        loop {
            let frames = {
                let memory = BackendMemory {
                    backend: RefCell::new(self.backend.as_mut()),
                };
                StackUnwinder::new(architecture, &self.symbols, &memory).unwind(thread, &registers, max_frames)?
            };
            let mut added = false;
            for frame in &frames {
                added |= self.load_symbols_for_address(frame.pc).unwrap_or(false);
            }
            if !added {
                return Ok(frames);
            }
        }
    }

    /// Map `address` to function names and source locations.
    ///
    /// Returns `Ok(None)` when no loaded image contains the address or it has no symbol.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, the backend's enumeration error, or [`DebuggerError::NoDebugInfo`]
    /// when the owning image has no usable DWARF.
    pub fn symbolicate(&mut self, address: Address) -> Result<Option<Symbolication>>
    {
        self.require_stopped()?;
        self.load_images()?;
        self.load_symbols_for_address(address)?;
        Ok(self.symbols.symbolicate(address))
    }
}
