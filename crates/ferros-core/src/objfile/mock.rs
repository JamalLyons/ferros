//! A made-up object format for tests that need symbol tables without a real format's
//! bytes.
//!
//! [`MockObjectFormat`] reads a tiny layout from the target's memory, so a test maps it
//! with [`MockBackend::with_region`](crate::backend::mock::MockBackend::with_region) and
//! can unmap or overwrite each part to exercise the session's caching and retry rules.
//! [`MockObjectImage`] builds the bytes. All addresses are absolute runtime addresses and
//! every integer is a little-endian `u64`:
//!
//! - **Header**, at the image's load address ([`MockObjectImage::header_bytes`]):
//!   [`MockObjectFormat::MAGIC`], four bytes of zero, then the code range
//!   `[code_start, code_end)`, the table address, the symbol count, and the image's
//!   mapping: the file address of its first byte and the size of its one segment, zero
//!   when it has none (56 bytes).
//! - **Table**, at the table address ([`MockObjectImage::table_bytes`]): per symbol its
//!   address, the address of its name and the name's length (24 bytes each).
//! - **Names**, at the address given to [`MockObjectImage::new`]
//!   ([`MockObjectImage::name_bytes`]): the names back to back, without terminators.
//!
//! A symbol names the pcs from its address to the next symbol's, within the code range.
//! Unlisted images are searched for at [`MockObjectFormat::ALIGNMENT`]; a probe reads the
//! [`MAGIC`](MockObjectFormat::MAGIC) word only, then the header.
//!
//! An image's file on disk, for [`ObjectFormat::open_image`], is a [`MockObjectFile`]:
//! [`MockObjectFile::FILE_MAGIC`], four bytes of zero, the file address of the image's
//! first byte, the size of its one segment and the number of entries; then each entry as
//! its kind, address and length followed by that many bytes. An entry is a DWARF section
//! (kind = its index in [`DWARF_SECTIONS`]), `eh_frame`,
//! `eh_frame_hdr`, `debug_frame`, an unwind hint for the function at its address, or the
//! file's build id. A file without `.debug_info` has no debug information: its
//! `eh_frame`, `eh_frame_hdr` and hints are still returned for unwinding, and its
//! `debug_frame` is dropped with the DWARF. Files are always arm64 and little-endian. A
//! file with a build id is another build than a slice that knows a different one, and
//! opening it for that slice fails; a file without one is never checked and its build id
//! is unknown.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use gimli::{RunTimeEndian, SectionId};

use super::{
    DWARF_SECTIONS, DebugSections, FunctionUnwind, ImageContents, ImageMapping, ImageProbe, ImageSegment, ImageSlice,
    MappedImage, MappedSymbols, ObjectFormat, SavedRegisterSlots, SectionData, SymbolIndex, SymbolKey, TargetMemory,
    UnwindHint, UnwindHints,
};
use crate::error::{DebuggerError, Result as EngineResult};
use crate::types::{Address, Architecture, BuildId, BuildIdentity};

/// The made-up format of the [module documentation](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct MockObjectFormat;

impl MockObjectFormat
{
    /// First word of a header.
    pub const MAGIC: [u8; 4] = *b"FMK1";
    /// Bytes in a header.
    pub const HEADER_SIZE: usize = 56;
    /// Bytes per table entry.
    pub const ENTRY_SIZE: usize = 24;
    /// Alignment of unlisted images.
    pub const ALIGNMENT: u64 = 0x1000;
    /// Most symbols a table may hold.
    const MAX_SYMBOLS: u64 = 1 << 16;

    /// A format to give a session ([`Session::new`](crate::Session::new)).
    #[must_use]
    pub fn shared() -> Arc<dyn ObjectFormat>
    {
        Arc::new(Self)
    }
}

fn word(bytes: &[u8], index: usize) -> u64
{
    let start = 8 + index * 8;
    bytes
        .get(start..start + 8)
        .and_then(|word| word.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

impl ObjectFormat for MockObjectFormat
{
    fn mapped_symbols(&self, memory: &mut dyn TargetMemory, load_address: Address)
    -> Result<Arc<dyn MappedSymbols>, String>
    {
        let header = memory
            .read(load_address, Self::HEADER_SIZE)
            .map_err(|error| format!("reading the mock header at {load_address}: {error}"))?;
        if header.get(..4) != Some(&Self::MAGIC[..]) {
            return Err(format!("no mock header at {load_address}"));
        }
        let code = word(&header, 0)..word(&header, 1);
        let count = word(&header, 3);
        if code.is_empty() || count > Self::MAX_SYMBOLS {
            return Err(format!("malformed mock header at {load_address}"));
        }
        Ok(Arc::new(MockSymbols {
            code,
            table_at: word(&header, 2),
            count,
        }))
    }

    fn unlisted_image_alignment(&self) -> Option<u64>
    {
        Some(Self::ALIGNMENT)
    }

    fn probe_unlisted_image(&self, memory: &mut dyn TargetMemory, at: Address) -> ImageProbe
    {
        match memory.read(at, Self::MAGIC.len()) {
            Err(_) => ImageProbe::Unreadable,
            Ok(magic) if magic != Self::MAGIC => ImageProbe::Nothing,
            Ok(_) => self.mapped_symbols(memory, at).map_or(ImageProbe::Nothing, ImageProbe::Image),
        }
    }

    /// The header's one segment, of file address `word 4` and size `word 5`. The slice
    /// names no part of a file, and its build id is unknown.
    fn map_image(&self, memory: &mut dyn TargetMemory, load_address: Address) -> MappedImage
    {
        let header = match memory.read(load_address, Self::HEADER_SIZE) {
            Ok(header) => header,
            Err(error) => {
                return MappedImage {
                    mapping: Err(format!("reading the mock header at {load_address}: {error}")),
                    slice: None,
                };
            }
        };
        if header.get(..4) != Some(&Self::MAGIC[..]) {
            return MappedImage {
                mapping: Err(format!("no mock header at {load_address}")),
                slice: None,
            };
        }
        let mapping = segment_mapping(word(&header, 4), word(&header, 5), load_address.value())
            .map_err(|error| format!("mock header at {load_address}: {error}"));
        MappedImage {
            mapping,
            slice: Some(ImageSlice::new(Vec::new(), BuildIdentity::Unknown)),
        }
    }

    /// A file whose build id is known and differs from the one `slice` knows is another
    /// build: [`DebuggerError::InvalidArgument`].
    fn open_image(&self, path: &Path, load_address: u64, slice: Option<&ImageSlice>) -> EngineResult<ImageContents>
    {
        let bytes = std::fs::read(path)?;
        let contents = MockObjectFile::parse(&bytes, path, load_address)?;
        if let (Some(file), Some(mapped)) = (contents.build_id.known(), slice.and_then(|slice| slice.build_id().known()))
            && file != mapped
        {
            return Err(DebuggerError::InvalidArgument(format!(
                "{} is another build than the mapped image: the file was replaced",
                path.display()
            )));
        }
        Ok(contents)
    }
}

fn segment_mapping(file_base: u64, size: u64, load_address: u64) -> EngineResult<ImageMapping>
{
    let segment = ImageSegment {
        address: file_base,
        size,
        file_offset: 0,
        file_size: size,
    };
    ImageMapping::from_segments(&[segment], load_address)
}

/// A mock image's code range and where its table lies.
struct MockSymbols
{
    code: Range<u64>,
    table_at: u64,
    count: u64,
}

impl MappedSymbols for MockSymbols
{
    fn contains(&self, address: Address) -> bool
    {
        self.code.contains(&address.value())
    }

    fn load(&self, memory: &mut dyn TargetMemory) -> Result<Box<dyn SymbolIndex>, String>
    {
        let at = Address::new(self.table_at);
        // `count` is bounded by `MAX_SYMBOLS`.
        let len = usize::try_from(self.count).unwrap_or(0) * MockObjectFormat::ENTRY_SIZE;
        let bytes = memory
            .read(at, len)
            .map_err(|error| format!("reading the mock table at {at}: {error}"))?;
        let mut symbols: Vec<(u64, u64, u64)> = bytes
            .as_chunks::<8>()
            .0
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[address, name, len]| (u64::from_le_bytes(address), u64::from_le_bytes(name), u64::from_le_bytes(len)))
            .filter(|&(address, ..)| self.code.contains(&address))
            .collect();
        symbols.sort_unstable();
        Ok(Box::new(MockIndex {
            code_end: self.code.end,
            symbols,
        }))
    }
}

/// A mock image's symbols, `(address, name address, name length)` sorted by address.
struct MockIndex
{
    code_end: u64,
    symbols: Vec<(u64, u64, u64)>,
}

impl SymbolIndex for MockIndex
{
    fn lookup(&self, address: Address) -> Option<SymbolKey>
    {
        let address = address.value();
        if address >= self.code_end {
            return None;
        }
        let index = self.symbols.partition_point(|&(start, ..)| start <= address).checked_sub(1)?;
        Some(SymbolKey(index as u64))
    }

    fn symbols(&self) -> Box<dyn Iterator<Item = (Address, SymbolKey)> + '_>
    {
        Box::new(
            self.symbols
                .iter()
                .enumerate()
                .map(|(index, &(address, ..))| (Address::new(address), SymbolKey(index as u64))),
        )
    }

    fn read_name(&self, memory: &mut dyn TargetMemory, key: SymbolKey) -> Result<Option<String>, String>
    {
        let Some(&(_, at, len)) = usize::try_from(key.0).ok().and_then(|index| self.symbols.get(index)) else {
            return Ok(None);
        };
        let at = Address::new(at);
        let len = usize::try_from(len).map_err(|_| format!("mock name at {at} is too long"))?;
        let bytes = memory
            .read(at, len)
            .map_err(|error| format!("reading a mock name at {at}: {error}"))?;
        Ok(String::from_utf8(bytes).ok().filter(|name| !name.is_empty()))
    }
}

/// The bytes of one [`MockObjectFormat`] image (see the [module documentation](self)).
#[derive(Debug, Clone)]
pub struct MockObjectImage
{
    code: Range<u64>,
    table_at: u64,
    names_at: u64,
    symbols: Vec<(u64, String)>,
    mapping: (u64, u64),
}

impl MockObjectImage
{
    /// An image whose code is `code`, with its table at `table_at` and its names at
    /// `names_at`, and no symbol yet.
    #[must_use]
    pub fn new(code: Range<u64>, table_at: u64, names_at: u64) -> Self
    {
        Self {
            code,
            table_at,
            names_at,
            symbols: Vec::new(),
            mapping: (0, 0),
        }
    }

    /// Add a symbol `name` at `address`.
    #[must_use]
    pub fn with_symbol(mut self, address: u64, name: &str) -> Self
    {
        self.symbols.push((address, name.to_owned()));
        self
    }

    /// Give the image one segment of `size` bytes whose first byte, the header, is at the
    /// file address `file_base`. Without one the image has no mapping.
    #[must_use]
    pub fn with_mapping(mut self, file_base: u64, size: u64) -> Self
    {
        self.mapping = (file_base, size);
        self
    }

    /// The header, to map at the image's load address.
    #[must_use]
    pub fn header_bytes(&self) -> Vec<u8>
    {
        let mut bytes = MockObjectFormat::MAGIC.to_vec();
        bytes.extend([0; 4]);
        for value in [
            self.code.start,
            self.code.end,
            self.table_at,
            self.symbols.len() as u64,
            self.mapping.0,
            self.mapping.1,
        ] {
            bytes.extend(value.to_le_bytes());
        }
        bytes
    }

    /// The table, to map at the table address.
    #[must_use]
    pub fn table_bytes(&self) -> Vec<u8>
    {
        let mut bytes = Vec::new();
        let mut name_at = self.names_at;
        for (address, name) in &self.symbols {
            for value in [*address, name_at, name.len() as u64] {
                bytes.extend(value.to_le_bytes());
            }
            name_at += name.len() as u64;
        }
        bytes
    }

    /// The names, to map at the names address.
    #[must_use]
    pub fn name_bytes(&self) -> Vec<u8>
    {
        self.symbols.iter().flat_map(|(_, name)| name.bytes()).collect()
    }
}

/// The file of one [`MockObjectFormat`] image (see the [module documentation](self)).
#[derive(Debug, Clone, Default)]
pub struct MockObjectFile
{
    file_base: u64,
    size: u64,
    entries: Vec<(u64, u64, Vec<u8>)>,
}

/// Entry kinds after the DWARF sections.
const EH_FRAME: u64 = 0x100;
const EH_FRAME_HDR: u64 = 0x101;
const DEBUG_FRAME: u64 = 0x102;
const BUILD_ID: u64 = 0x103;
const HINT_CFI: u64 = 0x200;
const HINT_LEAF: u64 = 0x201;
const HINT_FRAME_RECORD: u64 = 0x202;

fn encode_saved_slots(saved: SavedRegisterSlots) -> u64
{
    u64::from(saved.mask) | (u64::from(saved.first_offset.cast_unsigned()) << 32)
}

fn decode_saved_slots(value: u64) -> SavedRegisterSlots
{
    SavedRegisterSlots {
        mask: value as u32,
        first_offset: ((value >> 32) as u32).cast_signed(),
    }
}

impl MockObjectFile
{
    /// First word of a file.
    pub const FILE_MAGIC: [u8; 4] = *b"FMKF";

    /// A file whose image is one segment of `size` bytes, its first byte at the file
    /// address `file_base`, with no sections yet.
    #[must_use]
    pub fn new(file_base: u64, size: u64) -> Self
    {
        Self {
            file_base,
            size,
            entries: Vec::new(),
        }
    }

    /// Add the DWARF section `id` (one of [`DWARF_SECTIONS`]).
    ///
    /// # Panics
    /// When `id` is not one of [`DWARF_SECTIONS`].
    #[must_use]
    pub fn with_dwarf(mut self, id: SectionId, data: &[u8]) -> Self
    {
        let kind = DWARF_SECTIONS
            .iter()
            .position(|&known| known == id)
            .expect("a DWARF section the engine reads");
        self.entries.push((kind as u64, 0, data.to_vec()));
        self
    }

    /// Add `eh_frame` at the file address `address`.
    #[must_use]
    pub fn with_eh_frame(mut self, address: u64, data: &[u8]) -> Self
    {
        self.entries.push((EH_FRAME, address, data.to_vec()));
        self
    }

    /// Add `eh_frame_hdr` at the file address `address`.
    #[must_use]
    pub fn with_eh_frame_hdr(mut self, address: u64, data: &[u8]) -> Self
    {
        self.entries.push((EH_FRAME_HDR, address, data.to_vec()));
        self
    }

    /// Add `debug_frame` at the file address `address`.
    #[must_use]
    pub fn with_debug_frame(mut self, address: u64, data: &[u8]) -> Self
    {
        self.entries.push((DEBUG_FRAME, address, data.to_vec()));
        self
    }

    /// Add the unwind hint `hint` for the function starting at the file address `start`;
    /// it covers addresses up to the next hinted function.
    #[must_use]
    pub fn with_hint(mut self, start: u64, hint: UnwindHint) -> Self
    {
        let (kind, value, extra) = match hint {
            UnwindHint::Cfi { eh_frame_offset } => (HINT_CFI, eh_frame_offset as u64, None),
            UnwindHint::Leaf { stack_size, saved } => (HINT_LEAF, stack_size, Some(encode_saved_slots(saved))),
            UnwindHint::FrameRecord { saved } => (HINT_FRAME_RECORD, encode_saved_slots(saved), None),
        };
        let mut data = value.to_le_bytes().to_vec();
        if let Some(extra) = extra {
            data.extend(extra.to_le_bytes());
        }
        self.entries.push((kind, start, data));
        self
    }

    /// Record `id` as the file's build id. A file without one has an unknown build id.
    #[must_use]
    pub fn with_build_id(mut self, id: BuildId) -> Self
    {
        self.entries.push((BUILD_ID, 0, id.as_bytes().to_vec()));
        self
    }

    /// The file's bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8>
    {
        let mut bytes = Self::FILE_MAGIC.to_vec();
        bytes.extend([0; 4]);
        for value in [self.file_base, self.size, self.entries.len() as u64] {
            bytes.extend(value.to_le_bytes());
        }
        for (kind, address, data) in &self.entries {
            for value in [*kind, *address, data.len() as u64] {
                bytes.extend(value.to_le_bytes());
            }
            bytes.extend(data);
        }
        bytes
    }

    fn parse(bytes: &[u8], path: &Path, load_address: u64) -> EngineResult<ImageContents>
    {
        let invalid = || DebuggerError::InvalidArgument(format!("{} is not a mock object file", path.display()));
        if bytes.get(..4) != Some(&Self::FILE_MAGIC[..]) {
            return Err(invalid());
        }
        let mut at = 8;
        let mut next = || -> EngineResult<u64> {
            let value = bytes
                .get(at..at + 8)
                .and_then(|word| word.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(invalid)?;
            at += 8;
            Ok(value)
        };
        let (file_base, size, count) = (next()?, next()?, next()?);
        let mapping = segment_mapping(file_base, size, load_address)?;
        let mut dwarf = HashMap::new();
        let (mut eh_frame, mut eh_frame_hdr, mut debug_frame) = (None, None, None);
        let mut hints = Vec::new();
        let mut build_id = BuildIdentity::Unknown;
        for _ in 0..count {
            let mut next = || -> EngineResult<u64> {
                let value = bytes
                    .get(at..at + 8)
                    .and_then(|word| word.try_into().ok())
                    .map(u64::from_le_bytes)
                    .ok_or_else(invalid)?;
                at += 8;
                Ok(value)
            };
            let (kind, address, len) = (next()?, next()?, next()?);
            let len = usize::try_from(len).map_err(|_| invalid())?;
            let data: Arc<[u8]> = bytes.get(at..at + len).ok_or_else(invalid)?.into();
            at += len;
            let section = || SectionData {
                address,
                data: data.clone(),
            };
            let word_at = |offset: usize| {
                data.get(offset..offset + 8)
                    .and_then(|word| word.try_into().ok())
                    .map(u64::from_le_bytes)
            };
            match kind {
                EH_FRAME => eh_frame = Some(section()),
                EH_FRAME_HDR => eh_frame_hdr = Some(section()),
                DEBUG_FRAME => debug_frame = Some(section()),
                BUILD_ID => build_id = BuildId::new(&data).map(BuildIdentity::Known).ok_or_else(invalid)?,
                HINT_CFI | HINT_LEAF | HINT_FRAME_RECORD => {
                    let value = word_at(0).ok_or_else(invalid)?;
                    let hint = match kind {
                        HINT_CFI => UnwindHint::Cfi {
                            eh_frame_offset: usize::try_from(value).map_err(|_| invalid())?,
                        },
                        HINT_LEAF => UnwindHint::Leaf {
                            stack_size: value,
                            saved: if data.len() == 8 {
                                SavedRegisterSlots::default()
                            } else {
                                decode_saved_slots(word_at(8).ok_or_else(invalid)?)
                            },
                        },
                        _ => UnwindHint::FrameRecord {
                            saved: decode_saved_slots(value),
                        },
                    };
                    let start = mapping.relocate(address).ok_or_else(invalid)?;
                    hints.push((start, hint));
                }
                kind => {
                    let id = usize::try_from(kind)
                        .ok()
                        .and_then(|index| DWARF_SECTIONS.get(index))
                        .ok_or_else(invalid)?;
                    dwarf.insert(*id, data.clone());
                }
            }
        }
        let debug = if dwarf.contains_key(&SectionId::DebugInfo) {
            Ok(DebugSections { dwarf, debug_frame })
        } else {
            Err(DebuggerError::NoDebugInfo {
                path: path.to_path_buf(),
                detail: "mock object file has no .debug_info".into(),
            })
        };
        hints.sort_unstable_by_key(|&(start, _)| start);
        Ok(ImageContents {
            architecture: Architecture::Arm64,
            endian: RunTimeEndian::Little,
            build_id,
            mapping,
            eh_frame,
            eh_frame_hdr,
            unwind_hints: (!hints.is_empty()).then(|| Box::new(MockHints(hints)) as Box<dyn UnwindHints>),
            debug,
        })
    }
}

/// A mock file's hints, `(runtime start, hint)` sorted by start.
struct MockHints(Vec<(u64, UnwindHint)>);

impl UnwindHints for MockHints
{
    fn lookup(&self, address: Address) -> Option<FunctionUnwind>
    {
        let index = self
            .0
            .partition_point(|&(start, _)| start <= address.value())
            .checked_sub(1)?;
        let (start, hint) = self.0[index];
        Some(FunctionUnwind {
            start: Address::new(start),
            hint,
        })
    }
}

#[cfg(test)]
mod tests
{
    use std::collections::BTreeMap;

    use super::*;

    /// Memory of `(start, bytes)` regions.
    struct Regions(BTreeMap<u64, Vec<u8>>);

    impl TargetMemory for Regions
    {
        fn read(&mut self, at: Address, len: usize) -> Result<Vec<u8>, String>
        {
            let (start, bytes) = self.0.range(..=at.value()).next_back().ok_or("unmapped")?;
            let offset = usize::try_from(at.value() - start).map_err(|error| error.to_string())?;
            bytes
                .get(offset..offset + len)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "unmapped".to_owned())
        }
    }

    #[test]
    fn names_the_nearest_symbol_in_the_code()
    {
        let image = MockObjectImage::new(0x1000..0x1100, 0x2000, 0x3000)
            .with_symbol(0x1040, "bar")
            .with_symbol(0x1000, "foo");
        let mut memory = Regions(BTreeMap::from([
            (0x800, image.header_bytes()),
            (0x2000, image.table_bytes()),
            (0x3000, image.name_bytes()),
        ]));
        let symbols = MockObjectFormat
            .mapped_symbols(&mut memory, Address::new(0x800))
            .expect("header");
        assert!(symbols.contains(Address::new(0x10fc)));
        assert!(!symbols.contains(Address::new(0x1100)));
        let index = symbols.load(&mut memory).expect("table");
        let name = |memory: &mut Regions, address| {
            index
                .lookup(Address::new(address))
                .and_then(|key| index.read_name(memory, key).expect("name"))
        };
        assert_eq!(name(&mut memory, 0x1000).as_deref(), Some("foo"));
        assert_eq!(name(&mut memory, 0x1050).as_deref(), Some("bar"));
        assert_eq!(name(&mut memory, 0x1100), None);
        assert_eq!(index.symbols().count(), 2);
        assert!(matches!(
            MockObjectFormat.probe_unlisted_image(&mut memory, Address::new(0x800)),
            ImageProbe::Image(_)
        ));
        assert!(matches!(
            MockObjectFormat.probe_unlisted_image(&mut memory, Address::new(0x2000)),
            ImageProbe::Nothing
        ));
        assert!(matches!(
            MockObjectFormat.probe_unlisted_image(&mut memory, Address::new(0x9000)),
            ImageProbe::Unreadable
        ));
    }
}
