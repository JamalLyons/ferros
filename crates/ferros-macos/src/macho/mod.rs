//! The Mach-O implementation of [`ferros_core::objfile::ObjectFormat`].
//!
//! [`MachOFormat`] answers the engine's container questions for 64-bit little-endian
//! Mach-O images as mapped in a target:
//!
//! - **Symbol tables** (`symtab`): an image's `LC_SYMTAB`, `LC_FUNCTION_STARTS` and
//!   executable sections, read from its mapped header and `__LINKEDIT`, including images in
//!   the dyld shared cache, which have no file of their own.
//! - **Unlisted images**: code that no listed image contains (an image a backend leaves
//!   out, or code mapped outside every image) is named from the nearest `MH_MAGIC_64`
//!   header before it. Images in the dyld shared cache start on 4 KiB boundaries, finer
//!   than the 16 KiB page, so that is the alignment searched; the session decides which
//!   regions to search and caches what each probe found.
//! - **Mapped images** ([`image`]): the segment layout of a mapped header (without
//!   `__PAGEZERO`) and its [`SliceIdentity`] (CPU type, subtype and `LC_UUID`), which picks
//!   the slice of a universal file to read (#169, #175).
//! - **Image files** ([`image`], [`debuginfo`]): DWARF from the image, its `.dSYM`, a
//!   UUID-keyed cache or `dsymutil` ([`DebugInfoOptions`]); `__eh_frame`,
//!   `__eh_frame_hdr`, and `__unwind_info` (Apple compact unwind, `compact_unwind`) as
//!   the engine's unwind hints.
//!
//! Mach-O prefixes C-level names with an underscore (`_main`, `__ZN...`), which is not
//! part of the function's name; [`MappedSymbols::function_name`] removes it.

pub(crate) mod compact_unwind;
pub mod debuginfo;
pub mod image;
pub(crate) mod symtab;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferros_core::error::Result as EngineResult;
use ferros_core::objfile::{
    DebugInfoRetry, FileIdentityReader, ImageContents, ImageProbe, ImageSlice, MappedImage, MappedSymbols, ObjectFormat,
    SymbolIndex, SymbolKey, TargetMemory,
};
use ferros_core::types::Address;
use object::Endianness;
use object::macho::MachHeader64;
use object::read::macho::MachHeader as _;

pub use self::debuginfo::DebugInfoOptions;
pub use self::image::{SliceIdentity, file_mapping, slice_mapping};
use self::symtab::{SymbolTable, SymtabLayout};

/// `MH_MAGIC_64`, the first word of a 64-bit Mach-O header.
pub(crate) const MH_MAGIC_64: u32 = 0xfeed_facf;

/// Size of a 64-bit Mach-O header, which precedes its load commands.
pub const MACH_HEADER_SIZE: usize = 32;

/// Largest `sizeofcmds` read from a target.
const MAX_LOAD_COMMANDS_SIZE: usize = 1 << 20;

/// Alignment of Mach-O headers searched for: images in the dyld shared cache start on
/// 4 KiB boundaries.
const UNLISTED_IMAGE_ALIGNMENT: u64 = 0x1000;

/// Longest symbol name read; a longer one is cut off here.
const MAX_SYMBOL_NAME: usize = 4096;

/// The Mach-O [`ObjectFormat`]. See the [module documentation](self).
#[derive(Debug, Clone, Default)]
pub struct MachOFormat
{
    debug_info: DebugInfoOptions,
}

impl MachOFormat
{
    /// The format with the default [`DebugInfoOptions`].
    #[must_use]
    pub fn new() -> Self
    {
        Self::default()
    }

    /// The format with explicit companion debug-info discovery options.
    #[must_use]
    pub fn with_debug_info_options(debug_info: DebugInfoOptions) -> Self
    {
        Self { debug_info }
    }

    /// The format with the default options, as a session holds it.
    #[must_use]
    pub fn shared() -> Arc<dyn ObjectFormat>
    {
        Arc::new(Self::default())
    }

    /// This format as a session holds it.
    #[must_use]
    pub fn into_shared(self) -> Arc<dyn ObjectFormat>
    {
        Arc::new(self)
    }
}

/// Bytes of the header and load commands, given the first [`MACH_HEADER_SIZE`] bytes of a
/// 64-bit Mach-O header. The size is target-controlled: it is bounded before anyone
/// allocates it.
///
/// # Errors
/// A description of why `header` does not start a 64-bit Mach-O header, or why its load
/// commands are too large.
pub fn header_size(header: &[u8]) -> Result<usize, String>
{
    let mach = MachHeader64::<Endianness>::parse(header, 0).map_err(|error| format!("invalid image header: {error}"))?;
    let endian = mach.endian().map_err(|error| format!("invalid image header: {error}"))?;
    usize::try_from(mach.sizeofcmds(endian))
        .ok()
        .filter(|&commands| commands <= MAX_LOAD_COMMANDS_SIZE)
        .map(|commands| commands + MACH_HEADER_SIZE)
        .ok_or_else(|| "image load commands exceed the size limit".to_owned())
}

/// The Mach-O header and load commands mapped at `load_address`.
fn read_mapped_header(memory: &mut dyn TargetMemory, load_address: Address) -> Result<Vec<u8>, String>
{
    let header = memory
        .read(load_address, MACH_HEADER_SIZE)
        .map_err(|error| format!("reading the Mach-O header at {load_address}: {error}"))?;
    let size = header_size(&header).map_err(|error| format!("header at {load_address}: {error}"))?;
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

    /// The segments of the mapped header without `__PAGEZERO`, and its CPU type,
    /// subtype and `LC_UUID` as the slice. A header without `LC_UUID` leaves the build
    /// identity unknown, never absent (#170).
    fn map_image(&self, memory: &mut dyn TargetMemory, load_address: Address) -> MappedImage
    {
        let header = match read_mapped_header(memory, load_address) {
            Ok(header) => header,
            Err(detail) => {
                return MappedImage {
                    mapping: Err(detail),
                    slice: None,
                };
            }
        };
        let slice = SliceIdentity::from_header(&header).ok().map(|slice| slice.to_image_slice());
        let mapping = image::mapping_from_header(&header, load_address.value())
            .map_err(|error| format!("header at {load_address}: {error}"));
        MappedImage { mapping, slice }
    }

    fn open_image(&self, path: &Path, load_address: u64, slice: Option<&ImageSlice>) -> EngineResult<ImageContents>
    {
        self.open_image_with_debug_retry(path, load_address, slice)
            .map(|(contents, _)| contents)
    }

    fn open_image_with_debug_retry(
        &self,
        path: &Path,
        load_address: u64,
        slice: Option<&ImageSlice>,
    ) -> EngineResult<(ImageContents, Option<DebugInfoRetry>)>
    {
        let target = slice.and_then(SliceIdentity::from_image_slice);
        image::open(path, load_address, target.as_ref(), &self.debug_info)
    }

    /// The image and its `.dSYM` bundle's DWARF directory and members.
    fn load_inputs(&self, path: &Path) -> Vec<PathBuf>
    {
        debuginfo::load_inputs(path)
    }

    /// Device, file number and status-change time. A rewrite that keeps the size and
    /// restores the modification time still moves the status-change time (#346).
    fn file_identity_reader(&self) -> Option<FileIdentityReader>
    {
        Some(crate::file_identity::unix_file_identity)
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

    fn lookup_entry(&self, address: Address) -> Option<SymbolKey>
    {
        self.table.entry(address).map(|strx| SymbolKey(u64::from(strx)))
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
