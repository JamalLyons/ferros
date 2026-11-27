//! Stack traces and symbolication.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use super::Session;
use super::breakpoints::{TrapOriginals, overlay_traps};
use super::images::SessionSymtab;
use crate::backend::Backend;
use crate::error::{DebuggerError, Result};
use crate::objfile::{ImageProbe, MappedSymbols, TargetMemory};
use crate::symbols::Symbolication;
use crate::symbols::demangle::make_symbol_name;
use crate::symbols::unwind::{MemoryAccess, StackUnwinder, UnwoundStack};
use crate::types::{Address, FrameRegisters, MemoryRegion, StackFrame, SymbolName, SymbolSource};

/// [`MemoryAccess`] over a backend.
///
/// The unwinder reads through `&self` while [`Backend::read_memory`] needs `&mut`, so the
/// borrow is kept in a `RefCell`. The unwinder never reads re-entrantly, so the borrow
/// never conflicts.
///
/// Executable regions are listed once per trace, on the first question, so a trace that
/// never needs them (a single frame) does not ask the backend. A backend that cannot list
/// regions answers every address as executable.
///
/// Enabled software breakpoints read as their original bytes, as through
/// [`Session::read_memory`]: the unwinder decodes prologue and epilogue instructions, and
/// a stop at a breakpoint is often on one of them.
struct BackendMemory<'a>
{
    backend: RefCell<&'a mut dyn Backend>,
    executable: OnceCell<Option<Vec<MemoryRegion>>>,
    traps: TrapOriginals,
}

impl<'a> BackendMemory<'a>
{
    fn new(backend: &'a mut dyn Backend, traps: TrapOriginals) -> Self
    {
        Self {
            backend: RefCell::new(backend),
            executable: OnceCell::new(),
            traps,
        }
    }
}

impl MemoryAccess for BackendMemory<'_>
{
    fn read_u64(&self, address: Address) -> Result<u64>
    {
        let mut bytes = self.backend.borrow_mut().read_memory(address, 8)?;
        overlay_traps(&self.traps, address, &mut bytes);
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
    /// [`images`](Self::images)). A frame whose code has no DWARF (std linked without
    /// debug info, the dynamic loader, system libraries) is named from its
    /// image's symbol table instead, read from the target's memory through the session's
    /// [`ObjectFormat`](crate::objfile::ObjectFormat); code in no listed image is named
    /// from the image whose header precedes it in memory, if the format searches for one.
    /// Its symbol's [`source`](crate::types::SymbolName::source) is
    /// [`SymbolSource::SymbolTable`] and it has no location (see [`objfile`](crate::objfile)).
    /// A frame with neither has no symbol. Memory is read through the backend as
    /// little-endian 64-bit words.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::NoThreadSelected`] if the process has no
    /// thread, [`DebuggerError::Backend`] with `Unsupported` for an architecture without
    /// an [`Arch`](crate::arch::Arch), or an error from the backend.
    ///
    /// # Image list
    ///
    /// Each call refreshes the image list once ([`Backend::images`]) and uses that list
    /// for every frame of the trace; [`symbolicate`](Self::symbolicate) does the same per
    /// call. The list is deliberately not kept from one public call to the next, even
    /// within one stop: a backend may report images loaded or unloaded while the process
    /// is stopped (an attach, a mock that changes its list, a loader that another task
    /// drives), and a kept list would then resolve addresses against images that are gone.
    /// A caller that symbolicates many addresses of one stop pays one enumeration per call,
    /// and the symbol cache keeps each image's parsed symbols across those calls (#244).
    ///
    /// [`Backend::images`]: crate::backend::Backend::images
    pub fn stack_trace(&mut self, max_frames: usize) -> Result<Vec<StackFrame>>
    {
        let mut stack = self.unwind_stack(max_frames)?;
        self.name_frames_from_symbol_tables(&mut stack.frames, &stack.lookups);
        Ok(stack.frames)
    }

    /// Read the selected stopped thread's registers for a logical stack frame.
    ///
    /// `index` follows [`stack_trace`](Self::stack_trace), starting at zero and including
    /// inline frames. An inline frame uses its physical frame's registers and CFA.
    /// Returns `None` if the trace ends before `index`. Indices refer to the current stop.
    ///
    /// # Errors
    ///
    /// The same errors as [`stack_trace`](Self::stack_trace).
    pub fn frame_registers(&mut self, index: usize) -> Result<Option<FrameRegisters>>
    {
        let stack = self.unwind_stack(index.saturating_add(1))?;
        Ok(stack.frame_registers(index).cloned())
    }

    fn unwind_stack(&mut self, max_frames: usize) -> Result<UnwoundStack>
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
            let stack = {
                let traps = self.trap_originals();
                let memory = BackendMemory::new(self.backend.as_mut(), traps);
                StackUnwinder::new(&*arch, &self.symbols, &memory)
                    .with_runtime(self.runtime.as_ref())
                    .unwind_stack(thread, &registers, max_frames)?
            };
            let mut added = false;
            for (frame, &lookup) in stack.frames.iter().zip(&stack.lookups) {
                added |= self.load_symbols_for_frame(frame.pc);
                // A caller is symbolicated at its call instruction, which can lie in the
                // previous image when the call was the image's last instruction.
                added |= self.load_symbols_for_frame(lookup);
            }
            if !added {
                return Ok(stack);
            }
        }
    }

    /// Name each physical frame that has no DWARF symbol from its image's symbol table.
    ///
    /// `lookups` holds each frame's lookup address from the unwinder, in the order of
    /// `frames`: the address the frame's code is found at, which is its call instruction
    /// for a caller and its pc for a frame no call reached.
    fn name_frames_from_symbol_tables(&mut self, frames: &mut [StackFrame], lookups: &[Address])
    {
        // Images found by their header in this trace, for code the loader does not list.
        let mut unlisted = UnlistedImages::default();
        for (frame, &lookup) in frames.iter_mut().zip(lookups) {
            if frame.symbol.is_some() {
                continue;
            }
            frame.symbol = self
                .symbol_table_name(lookup)
                .or_else(|| self.unlisted_symbol_table_name(&mut unlisted, lookup));
        }
    }

    /// Name `address` from the symbol table of an image the backend does not list, found
    /// by the nearest image header before `address` in its executable region.
    ///
    /// This is a fallback for code no listed image contains. A backend that lists every
    /// image, the dynamic loader included, needs no search for the base of a main-thread
    /// trace (the loader's entry point); a backend that leaves out an image, or code
    /// mapped outside every image, still gets one. The header is searched for at the format's
    /// [`unlisted_image_alignment`](crate::objfile::ObjectFormat::unlisted_image_alignment),
    /// from `address` back to the start of its region; a format without one gets no
    /// search. A
    /// candidate the format does not recognize, or whose code does not contain `address`,
    /// is skipped. Each probed address's result is kept for the rest of the trace, so
    /// later frames in the same region reuse the probes instead of reading again; a search
    /// that failed for one pc does not stop another pc's search.
    fn unlisted_symbol_table_name(&mut self, unlisted: &mut UnlistedImages, address: Address) -> Option<SymbolName>
    {
        // A backend that lists no images (one that cannot, or a mock) gets no search either.
        if self.loaded_images.is_empty() {
            return None;
        }
        let alignment = self.format.unlisted_image_alignment()?;
        let request = self.symbol_request;
        if let Some(entry) = unlisted.images.iter_mut().find(|entry| entry.symbols.contains(address)) {
            return name_in(self.backend.as_mut(), entry, address, request);
        }
        if unlisted.regions.is_none() {
            match self.backend.memory_regions() {
                Ok(regions) => {
                    unlisted.regions = Some(
                        regions
                            .into_iter()
                            .filter(|region| region.permissions.contains('x'))
                            .collect(),
                    );
                }
                Err(error) => {
                    tracing::debug!("no image header search for {address}: listing memory regions: {error}");
                    unlisted.regions = Some(Vec::new());
                }
            }
        }
        let region = unlisted
            .regions
            .as_deref()?
            .iter()
            .find(|region| region.start <= address && address < region.end)?;
        let start = region.start.value();
        tracing::debug!("searching for the header of an unlisted image containing {address}");
        let symbols = self.search_image_header(&mut unlisted.probes, alignment, start, address)?;
        unlisted.images.push(SessionSymtab::new(symbols));
        let entry = unlisted.images.last_mut()?;
        name_in(self.backend.as_mut(), entry, address, request)
    }

    /// The symbol table of the nearest image header at or before `address`, on an
    /// `alignment` boundary no lower than `start`, whose code contains `address`.
    fn search_image_header(
        &mut self,
        probes: &mut HashMap<u64, ImageProbe>,
        alignment: u64,
        start: u64,
        address: Address,
    ) -> Option<Arc<dyn MappedSymbols>>
    {
        if !alignment.is_power_of_two() {
            tracing::debug!("no image header search: alignment {alignment:#x} is not a power of two");
            return None;
        }
        let mut at = address.value() & !(alignment - 1);
        for _ in 0..MAX_HEADER_SEARCH_STEPS {
            if at < start {
                return None;
            }
            let probe = match probes.get(&at) {
                Some(probe) => probe.clone(),
                None => {
                    let probe = self
                        .format
                        .probe_unlisted_image(&mut BackendTarget(self.backend.as_mut()), Address::new(at));
                    probes.insert(at, probe.clone());
                    probe
                }
            };
            match probe {
                ImageProbe::Image(symbols) if symbols.contains(address) => return Some(symbols),
                ImageProbe::Image(_) => tracing::debug!("image header at 0x{at:x} does not contain {address}"),
                ImageProbe::Nothing => {}
                ImageProbe::Unreadable => {
                    tracing::debug!("image header search for {address} stops at unreadable 0x{at:x}");
                    return None;
                }
            }
            at = at.checked_sub(alignment)?;
        }
        None
    }

    /// Load a frame's image: its unwind tables, and its source debug information when it
    /// has any. Returns whether the image was newly loaded, even without debug
    /// information, since its unwind tables alone can reveal further callers. A failure
    /// leaves the frame without a symbol and is logged, since a stack trace still returns
    /// raw frames.
    fn load_symbols_for_frame(&mut self, pc: Address) -> bool
    {
        let loaded = self.load_executable_for_address(pc).unwrap_or_else(|error| {
            tracing::debug!("stack trace continues without unwind tables at {pc}: {error}");
            false
        });
        if let Err(error) = self.load_symbols_for_address(pc) {
            tracing::debug!("stack trace continues without source symbols at {pc}: {error}");
        }
        loaded
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
    /// another reason.
    ///
    /// # Failed loads
    ///
    /// A load failure is kept with a stamp (size and timestamps) of the image and its
    /// companion inputs. Later calls, including [`stack_trace`](Self::stack_trace), retry
    /// when one of those inputs appears, disappears or changes. If only the companion
    /// inputs changed and the executable was loaded, discovery can retry over the cached
    /// unwind information. A failure to open the executable can also be retried when the
    /// mapped build id becomes known. A file rewritten with the same size and timestamps
    /// is not noticed.
    ///
    /// Like [`stack_trace`](Self::stack_trace), each call refreshes the image list once.
    pub fn symbolicate(&mut self, address: Address) -> Result<Option<Symbolication>>
    {
        self.require_stopped()?;
        self.load_images()?;
        self.begin_symbol_request();
        self.load_symbols_for_address(address)?;
        Ok(self.symbols.symbolicate(address))
    }
}

impl Session
{
    /// The name of the function containing `address` from the symbol table of the image
    /// mapped there, marked [`SymbolSource::SymbolTable`]. `None` when no image's code
    /// contains it, or the table could not be read (logged, and tried again by the next
    /// request).
    ///
    /// The image is found by its mapped header rather than by [`ImageMapping`], which does
    /// not map shared-cache images; each image's table is read once and its names as
    /// needed.
    ///
    /// [`ImageMapping`]: crate::symbols::ImageMapping
    fn symbol_table_name(&mut self, address: Address) -> Option<SymbolName>
    {
        let request = self.symbol_request;
        for index in 0..self.loaded_images.len() {
            let image = &self.loaded_images[index];
            if image.symtab.is_none() && image.symtab_failed_in_request != Some(request) {
                let load_address = Address::new(image.descriptor_load_address());
                let symbols = self
                    .format
                    .mapped_symbols(&mut BackendTarget(self.backend.as_mut()), load_address);
                let image = &mut self.loaded_images[index];
                match symbols {
                    Ok(symbols) => {
                        image.symtab = Some(SessionSymtab::new(symbols));
                        image.symtab_failed_in_request = None;
                    }
                    Err(detail) => {
                        tracing::debug!("no symbol table for the image at {load_address}: {detail}");
                        image.symtab_failed_in_request = Some(request);
                    }
                }
            }
            let backend = self.backend.as_mut();
            let Some(entry) = &mut self.loaded_images[index].symtab else {
                continue;
            };
            if entry.symbols.contains(address) {
                return name_in(backend, entry, address, request);
            }
        }
        None
    }
}

/// Images found by their header during one trace (see
/// [`Session::unlisted_symbol_table_name`]).
#[derive(Default)]
struct UnlistedImages
{
    images: Vec<SessionSymtab>,
    /// What each address probed for a header held.
    probes: HashMap<u64, ImageProbe>,
    /// The executable regions, listed on the first search.
    regions: Option<Vec<MemoryRegion>>,
}

/// Steps searched back for an unlisted image's header, each one the format's
/// [`unlisted_image_alignment`](crate::objfile::ObjectFormat::unlisted_image_alignment)
/// (16 MiB of code at 4 KiB).
const MAX_HEADER_SEARCH_STEPS: usize = 4096;

/// [`TargetMemory`] over a backend, for the session's
/// [`ObjectFormat`](crate::objfile::ObjectFormat). Reads are the target's bytes as mapped:
/// an image's header and symbol table never hold a trap.
pub(super) struct BackendTarget<'a>(pub(super) &'a mut dyn Backend);

impl TargetMemory for BackendTarget<'_>
{
    fn read(&mut self, at: Address, len: usize) -> std::result::Result<Vec<u8>, String>
    {
        self.0.read_memory(at, len).map_err(|error| error.to_string())
    }
}

/// The symbol table name of `address` in the image of `entry`, reading the table and the
/// name from the target as needed. Read failures are logged and not kept, except that
/// the table is not read again within `request`.
fn name_in(backend: &mut dyn Backend, entry: &mut SessionSymtab, address: Address, request: u64) -> Option<SymbolName>
{
    let mut memory = BackendTarget(backend);
    if entry.index.is_none() {
        if entry.index_failed_in_request == Some(request) {
            return None;
        }
        match entry.symbols.load(&mut memory) {
            Ok(index) => {
                entry.index = Some(index);
                entry.index_failed_in_request = None;
            }
            Err(detail) => {
                tracing::debug!("no symbol table name for {address}: {detail}");
                entry.index_failed_in_request = Some(request);
                return None;
            }
        }
    }
    let index = entry.index.as_ref()?;
    let key = index.lookup(address)?;
    let raw = if let Some(raw) = entry.names.get(&key) {
        raw.clone()
    } else {
        match index.read_name(&mut memory, key) {
            Ok(raw) => {
                entry.names.insert(key, raw.clone());
                raw
            }
            Err(detail) => {
                tracing::debug!("no symbol table name for {address}: {detail}");
                None
            }
        }
    }?;
    let name = entry.symbols.function_name(&raw).to_owned();
    Some(make_symbol_name(name, SymbolSource::SymbolTable))
}
