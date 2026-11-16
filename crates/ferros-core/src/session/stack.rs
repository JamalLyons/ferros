//! Stack traces and symbolication.

use std::cell::{OnceCell, RefCell};

use super::Session;
use super::images::SessionSymtab;
use crate::backend::Backend;
use crate::error::{DebuggerError, Result};
use crate::symbols::Symbolication;
use crate::symbols::demangle::make_symbol_name;
use crate::symbols::symtab::{self, SymtabLayout};
use crate::symbols::unwind::{MemoryAccess, StackUnwinder};
use crate::types::{Address, MemoryRegion, StackFrame, SymbolName, SymbolSource};

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
    /// [`images`](Self::images)). A frame whose code has no DWARF (std linked without
    /// debug info, dyld, the system libraries in the dyld shared cache) is named from its
    /// image's Mach-O symbol table instead, read from the target's memory: its symbol's
    /// [`source`](crate::types::SymbolName::source) is
    /// [`SymbolSource::SymbolTable`](crate::types::SymbolSource::SymbolTable) and it has
    /// no location (see [`symbols::symtab`](crate::symbols)). A frame with neither has
    /// no symbol. Memory is read through the backend as little-endian 64-bit words.
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
                let mut frames = frames;
                self.name_frames_from_symbol_tables(&mut frames);
                return Ok(frames);
            }
        }
    }

    /// Name each physical frame that has no DWARF symbol from its image's symbol table.
    ///
    /// Such a frame has no inlined frames, so it is the innermost frame exactly when its
    /// index is zero; any other frame is looked up at its call instruction, as the
    /// unwinder does.
    fn name_frames_from_symbol_tables(&mut self, frames: &mut [StackFrame])
    {
        // Images found by their header in this trace, for code the loader does not list.
        let mut unlisted: Vec<SessionSymtab> = Vec::new();
        for frame in frames.iter_mut().filter(|frame| frame.symbol.is_none()) {
            let lookup = if frame.index == 0 {
                frame.pc
            } else {
                Address::new(frame.pc.value().saturating_sub(4))
            };
            frame.symbol = self
                .symbol_table_name(lookup)
                .or_else(|| self.unlisted_symbol_table_name(&mut unlisted, lookup));
        }
    }

    /// Name `address` from the symbol table of an image the backend does not list, found
    /// by the nearest Mach-O header before `address` in its executable region.
    ///
    /// dyld leaves itself out of the image list it publishes, yet its `start` is the base
    /// of every main-thread trace (on current macOS dyld itself lives in the shared cache).
    /// The header is searched for at 4 KiB boundaries, from `address` back to the start of
    /// its region, and the nearest one is accepted only when its executable sections
    /// contain `address`.
    fn unlisted_symbol_table_name(&mut self, unlisted: &mut Vec<SessionSymtab>, address: Address) -> Option<SymbolName>
    {
        // A backend that lists no images (one that cannot, or a mock) gets no search either.
        if self.loaded_images.is_empty() {
            return None;
        }
        if let Some(entry) = unlisted.iter_mut().find(|entry| entry.layout.contains(address)) {
            return name_in(self.backend.as_mut(), entry, address);
        }
        let regions = self.backend.memory_regions().ok()?;
        let region = regions
            .iter()
            .find(|region| region.permissions.contains('x') && region.start <= address && address < region.end)?;
        let start = region.start.value();
        let mut page = address.value() & !(PAGE_SIZE - 1);
        for _ in 0..MAX_HEADER_SEARCH_PAGES {
            if page < start {
                return None;
            }
            let magic = self.backend.read_memory(Address::new(page), 4).ok()?;
            if magic == MH_MAGIC_64.to_le_bytes() {
                let layout = self
                    .read_mapped_header(page)
                    .ok()
                    .and_then(|header| SymtabLayout::from_header(&header, page).ok())
                    .filter(|layout| layout.contains(address))?;
                unlisted.push(SessionSymtab {
                    layout,
                    table: None,
                    names: std::collections::HashMap::new(),
                });
                let entry = unlisted.last_mut()?;
                return name_in(self.backend.as_mut(), entry, address);
            }
            page = page.checked_sub(PAGE_SIZE)?;
        }
        None
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

impl Session
{
    /// The name of the function containing `address` from the symbol table of the image
    /// mapped there, marked [`SymbolSource::SymbolTable`]. `None` when no image's code
    /// contains it, or the table could not be read (logged, and kept for the image).
    ///
    /// The image is found by its mapped header rather than by [`ImageMapping`], which does
    /// not map shared-cache images; each image's table is read once and its names as
    /// needed.
    ///
    /// [`ImageMapping`]: crate::symbols::ImageMapping
    fn symbol_table_name(&mut self, address: Address) -> Option<SymbolName>
    {
        for index in 0..self.loaded_images.len() {
            if self.loaded_images[index].symtab.is_none() {
                let load_address = self.loaded_images[index].descriptor_load_address();
                let layout = self
                    .read_mapped_header(load_address)
                    .and_then(|header| SymtabLayout::from_header(&header, load_address).map_err(|error| error.to_string()));
                self.loaded_images[index].symtab = Some(layout.map(|layout| SessionSymtab {
                    layout,
                    table: None,
                    names: std::collections::HashMap::new(),
                }));
            }
            let Some(Ok(entry)) = &self.loaded_images[index].symtab else {
                continue;
            };
            if !entry.layout.contains(address) {
                continue;
            }
            return self.symbol_table_name_in(index, address);
        }
        None
    }

    /// [`symbol_table_name`](Self::symbol_table_name) within the image at `index`, whose
    /// code contains `address`.
    fn symbol_table_name_in(&mut self, index: usize, address: Address) -> Option<SymbolName>
    {
        let backend = self.backend.as_mut();
        let Some(Ok(entry)) = &mut self.loaded_images[index].symtab else {
            return None;
        };
        name_in(backend, entry, address)
    }
}

/// Alignment of Mach-O headers searched for: images in the dyld shared cache start on
/// 4 KiB boundaries, finer than the 16 KiB page.
const PAGE_SIZE: u64 = 0x1000;

/// Steps searched back for an unlisted image's header: 16 MiB of code.
const MAX_HEADER_SEARCH_PAGES: usize = 4096;

/// `MH_MAGIC_64`, the first word of a 64-bit Mach-O header.
const MH_MAGIC_64: u32 = 0xfeed_facf;

/// The symbol table name of `address` in the image of `entry`, reading the table and the
/// name from the target as needed.
fn name_in(backend: &mut dyn Backend, entry: &mut SessionSymtab, address: Address) -> Option<SymbolName>
{
    let table = entry.table.get_or_insert_with(|| {
        let (at, size) = entry.layout.nlist_range();
        backend
            .read_memory(at, size)
            .map(|bytes| entry.layout.parse_symbols(&bytes))
            .map_err(|error| format!("reading the symbol table at {at}: {error}"))
    });
    let table = match table {
        Ok(table) => table,
        Err(detail) => {
            tracing::debug!("no symbol table name for {address}: {detail}");
            return None;
        }
    };
    let strx = table.lookup(&entry.layout, address)?;
    let layout = &entry.layout;
    let raw = entry
        .names
        .entry(strx)
        .or_insert_with(|| read_symbol_string(backend, layout, strx))
        .clone()?;
    let name = symtab::strip_symbol_prefix(&raw).to_owned();
    Some(make_symbol_name(name).with_source(SymbolSource::SymbolTable))
}

/// Longest symbol name read; a longer one is cut off here.
const MAX_SYMBOL_NAME: usize = 4096;

/// Read the NUL-terminated string at `strx` of an image's string table, in pages so a
/// short name near the end of mapped memory still reads.
fn read_symbol_string(backend: &mut dyn Backend, layout: &SymtabLayout, strx: u32) -> Option<String>
{
    const CHUNK: usize = 256;
    let (start, available) = layout.string_range(strx)?;
    let limit = available.min(MAX_SYMBOL_NAME);
    let mut bytes = Vec::new();
    while bytes.len() < limit {
        let offset = u64::try_from(bytes.len()).ok()?;
        let at = Address::new(start.value().wrapping_add(offset));
        // Stay within one page so a read never crosses into unmapped memory needlessly.
        let to_page = usize::try_from(0x4000 - (at.value() & 0x3fff)).ok()?;
        let length = CHUNK.min(limit - bytes.len()).min(to_page);
        let chunk = backend.read_memory(at, length).ok()?;
        if let Some(end) = chunk.iter().position(|&byte| byte == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return String::from_utf8(bytes).ok().filter(|name| !name.is_empty());
        }
        if chunk.is_empty() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).ok().filter(|name| !name.is_empty())
}
