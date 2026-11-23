//! The Mach-O implementation of [`ferros_core::objfile::ObjectFormat`].
//!
//! [`MachOFormat`] answers the engine's container questions for 64-bit little-endian
//! Mach-O images as mapped in a target:
//!
//! - **Symbol tables** ([`symtab`]): an image's `LC_SYMTAB`, `LC_FUNCTION_STARTS` and
//!   executable sections, read from its mapped header and `__LINKEDIT`, including images in
//!   the dyld shared cache, which have no file of their own.
//! - **Unlisted images**: code that no listed image contains (an image a backend leaves
//!   out, or code mapped outside every image) is named from the nearest `MH_MAGIC_64`
//!   header before it. Images in the dyld shared cache start on 4 KiB boundaries, finer
//!   than the 16 KiB page, so that is the alignment searched; the session decides which
//!   regions to search and caches what each probe found.
//!
//! Mach-O prefixes C-level names with an underscore (`_main`, `__ZN...`), which is not
//! part of the function's name; [`MappedSymbols::function_name`] removes it.

pub(crate) mod symtab;

use std::sync::Arc;

use ferros_core::objfile::{ImageProbe, MappedSymbols, ObjectFormat, SymbolIndex, SymbolKey, TargetMemory};
use ferros_core::types::Address;
use object::Endianness;
use object::macho::MachHeader64;
use object::read::macho::MachHeader as _;

use self::symtab::{SymbolTable, SymtabLayout};

/// `MH_MAGIC_64`, the first word of a 64-bit Mach-O header.
pub(crate) const MH_MAGIC_64: u32 = 0xfeed_facf;

/// Size of a 64-bit Mach-O header, which precedes its load commands.
const MACH_HEADER_SIZE: usize = 32;

/// Largest `sizeofcmds` read from a target.
const MAX_LOAD_COMMANDS_SIZE: usize = 1 << 20;

/// Alignment of Mach-O headers searched for: images in the dyld shared cache start on
/// 4 KiB boundaries.
const UNLISTED_IMAGE_ALIGNMENT: u64 = 0x1000;

/// Longest symbol name read; a longer one is cut off here.
const MAX_SYMBOL_NAME: usize = 4096;

/// The Mach-O [`ObjectFormat`]. See the [module documentation](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct MachOFormat;

impl MachOFormat
{
    /// The format as a session holds it.
    #[must_use]
    pub fn shared() -> Arc<dyn ObjectFormat>
    {
        Arc::new(Self)
    }
}

/// The Mach-O header and load commands mapped at `load_address`.
fn read_mapped_header(memory: &mut dyn TargetMemory, load_address: Address) -> Result<Vec<u8>, String>
{
    let header = memory
        .read(load_address, MACH_HEADER_SIZE)
        .map_err(|error| format!("reading the Mach-O header at {load_address}: {error}"))?;
    let mach = MachHeader64::<Endianness>::parse(&*header, 0)
        .map_err(|error| format!("header at {load_address}: invalid image header: {error}"))?;
    let endian = mach
        .endian()
        .map_err(|error| format!("header at {load_address}: invalid image header: {error}"))?;
    // The size is target-controlled: bound the read before anyone allocates it.
    let size = usize::try_from(mach.sizeofcmds(endian))
        .ok()
        .filter(|&commands| commands <= MAX_LOAD_COMMANDS_SIZE)
        .map(|commands| commands + MACH_HEADER_SIZE)
        .ok_or_else(|| format!("header at {load_address}: image load commands exceed the size limit"))?;
    memory
        .read(load_address, size)
        .map_err(|error| format!("reading {size} bytes of load commands at {load_address}: {error}"))
}

fn mapped_layout(memory: &mut dyn TargetMemory, load_address: Address) -> Result<Arc<MachOSymbols>, String>
{
    let header = read_mapped_header(memory, load_address)?;
    let layout = SymtabLayout::from_header(&header, load_address.value()).map_err(|error| error.to_string())?;
    Ok(Arc::new(MachOSymbols { layout }))
}

impl ObjectFormat for MachOFormat
{
    fn mapped_symbols(&self, memory: &mut dyn TargetMemory, load_address: Address)
    -> Result<Arc<dyn MappedSymbols>, String>
    {
        Ok(mapped_layout(memory, load_address)?)
    }

    fn unlisted_image_alignment(&self) -> Option<u64>
    {
        Some(UNLISTED_IMAGE_ALIGNMENT)
    }

    /// Whether a usable Mach-O header starts at `at`: an `MH_MAGIC_64` word whose header
    /// parses into a symbol table layout.
    fn probe_unlisted_image(&self, memory: &mut dyn TargetMemory, at: Address) -> ImageProbe
    {
        let magic = match memory.read(at, 4) {
            Ok(magic) => magic,
            Err(error) => {
                tracing::debug!("image header probe at {at}: {error}");
                return ImageProbe::Unreadable;
            }
        };
        if magic != MH_MAGIC_64.to_le_bytes() {
            return ImageProbe::Nothing;
        }
        match mapped_layout(memory, at) {
            Ok(symbols) => ImageProbe::Image(symbols),
            Err(detail) => {
                tracing::debug!("skipping a candidate image header at {at}: {detail}");
                ImageProbe::Nothing
            }
        }
    }
}

/// One mapped image's symbol table layout.
struct MachOSymbols
{
    layout: SymtabLayout,
}

impl MappedSymbols for MachOSymbols
{
    fn contains(&self, address: Address) -> bool
    {
        self.layout.contains(address)
    }

    /// Read the image's `nlist` entries and, when it records them, its function starts.
    fn load(&self, memory: &mut dyn TargetMemory) -> Result<Box<dyn SymbolIndex>, String>
    {
        let (at, size) = self.layout.nlist_range();
        let nlists = memory
            .read(at, size)
            .map_err(|error| format!("reading the symbol table at {at}: {error}"))?;
        let mut table = self.layout.parse_symbols(&nlists);
        if let Some((at, size)) = self.layout.function_starts_range() {
            let starts = memory
                .read(at, size)
                .map_err(|error| format!("reading the function starts at {at}: {error}"))?;
            table = table.with_function_starts(self.layout.parse_function_starts(&starts));
        }
        Ok(Box::new(MachOIndex {
            layout: self.layout.clone(),
            table,
        }))
    }

    fn function_name<'a>(&self, raw: &'a str) -> &'a str
    {
        symtab::strip_symbol_prefix(raw)
    }
}

/// One image's parsed symbol table. A [`SymbolKey`] is a string table index.
struct MachOIndex
{
    layout: SymtabLayout,
    table: SymbolTable,
}

impl SymbolIndex for MachOIndex
{
    fn lookup(&self, address: Address) -> Option<SymbolKey>
    {
        self.table
            .lookup(&self.layout, address)
            .map(|strx| SymbolKey(u64::from(strx)))
    }

    fn symbols(&self) -> Box<dyn Iterator<Item = (Address, SymbolKey)> + '_>
    {
        Box::new(
            self.table
                .symbols()
                .map(|(address, strx)| (address, SymbolKey(u64::from(strx)))),
        )
    }

    /// Read the NUL-terminated string at the key's string table index, in pages so a
    /// short name near the end of mapped memory still reads.
    fn read_name(&self, memory: &mut dyn TargetMemory, key: SymbolKey) -> Result<Option<String>, String>
    {
        const CHUNK: usize = 256;
        let Some((start, available)) = u32::try_from(key.0).ok().and_then(|strx| self.layout.string_range(strx)) else {
            return Ok(None);
        };
        let limit = available.min(MAX_SYMBOL_NAME);
        let mut bytes = Vec::new();
        while bytes.len() < limit {
            let at = Address::new(start.value().wrapping_add(bytes.len() as u64));
            // Stay within one page so a read never crosses into unmapped memory needlessly.
            let to_page = usize::try_from(0x4000 - (at.value() & 0x3fff)).unwrap_or(CHUNK);
            let length = CHUNK.min(limit - bytes.len()).min(to_page);
            let chunk = memory
                .read(at, length)
                .map_err(|error| format!("reading a symbol name at {at}: {error}"))?;
            if let Some(end) = chunk.iter().position(|&byte| byte == 0) {
                bytes.extend_from_slice(&chunk[..end]);
                break;
            }
            if chunk.is_empty() {
                return Err(format!("reading a symbol name at {at}: no bytes"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8(bytes).ok().filter(|name| !name.is_empty()))
    }
}

#[cfg(test)]
mod tests;
