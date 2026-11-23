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
//!   `[code_start, code_end)`, the table address and the symbol count (40 bytes).
//! - **Table**, at the table address ([`MockObjectImage::table_bytes`]): per symbol its
//!   address, the address of its name and the name's length (24 bytes each).
//! - **Names**, at the address given to [`MockObjectImage::new`]
//!   ([`MockObjectImage::name_bytes`]): the names back to back, without terminators.
//!
//! A symbol names the pcs from its address to the next symbol's, within the code range.
//! Unlisted images are searched for at [`MockObjectFormat::ALIGNMENT`]; a probe reads the
//! [`MAGIC`](MockObjectFormat::MAGIC) word only, then the header.

use std::ops::Range;
use std::sync::Arc;

use super::{ImageProbe, MappedSymbols, ObjectFormat, SymbolIndex, SymbolKey, TargetMemory};
use crate::types::Address;

/// The made-up format of the [module documentation](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct MockObjectFormat;

impl MockObjectFormat
{
    /// First word of a header.
    pub const MAGIC: [u8; 4] = *b"FMK1";
    /// Bytes in a header.
    pub const HEADER_SIZE: usize = 40;
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
        }
    }

    /// Add a symbol `name` at `address`.
    #[must_use]
    pub fn with_symbol(mut self, address: u64, name: &str) -> Self
    {
        self.symbols.push((address, name.to_owned()));
        self
    }

    /// The header, to map at the image's load address.
    #[must_use]
    pub fn header_bytes(&self) -> Vec<u8>
    {
        let mut bytes = MockObjectFormat::MAGIC.to_vec();
        bytes.extend([0; 4]);
        for value in [self.code.start, self.code.end, self.table_at, self.symbols.len() as u64] {
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
