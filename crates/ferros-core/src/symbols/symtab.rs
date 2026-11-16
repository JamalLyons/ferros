//! Function names from a Mach-O image's symbol table (`LC_SYMTAB`), for code without DWARF.
//!
//! Some code has no DWARF: Rust's standard library linked without debug info
//! (`std::rt::lang_start_internal`), the C `main` shim, dyld's `start`, and the system
//! libraries in the dyld shared cache (`libsystem_pthread`'s `_pthread_start` and
//! `thread_start`). Its function still has a symbol table entry, and the nearest defined
//! symbol at or before a pc names the function containing it.
//!
//! The table is read from the image as mapped in the target, not from the file on disk:
//! [`SymtabLayout::from_header`] parses the mapped load commands, and the `nlist` entries
//! and strings are read from the runtime copy of `__LINKEDIT`. `symoff` and `stroff` are
//! file offsets; `__LINKEDIT`'s `fileoff` relates them to its `vmaddr`. This works the
//! same for an image in the dyld shared cache, whose offsets are offsets in the cache file
//! and whose `__LINKEDIT` is the cache's shared one; such an image has no file on disk to
//! read at all. The cache keeps only the exported and global symbols in that table (its
//! local symbols live in a separate `.symbols` file), so a static function in a cache
//! library is named after the nearest preceding exported one. The names that a stack
//! trace's base frames need (`_pthread_start`, `thread_start`) are exported.
//!
//! Only symbols in executable sections (`S_ATTR_PURE_INSTRUCTIONS` or
//! `S_ATTR_SOME_INSTRUCTIONS`) count, and only for a pc in the same section, so a pc
//! never takes a name from data or from a symbol in another section.

use object::macho::{MachHeader64, N_SECT, N_STAB, N_TYPE, Nlist64, S_ATTR_PURE_INSTRUCTIONS, S_ATTR_SOME_INSTRUCTIONS};
use object::read::macho::{MachHeader as _, Section as _, Segment as _};
use object::{Endianness, LittleEndian, pod};

use crate::error::{DebuggerError, Result};
use crate::types::Address;

/// Size of one 64-bit `nlist` entry.
pub(crate) const NLIST_SIZE: usize = 16;

/// Largest symbol table read ([`SymtabLayout::nlist_range`]): 4 Mi entries, 64 MiB.
const MAX_SYMBOLS: u32 = 1 << 22;

/// Where an image's symbol table lies in the target, and its executable sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymtabLayout
{
    /// Runtime ranges `[start, end)` of executable sections, with their 1-based index.
    sections: Vec<(u8, u64, u64)>,
    /// Added (wrapping) to a file virtual address to get the runtime address.
    slide: u64,
    symbols_at: u64,
    symbol_count: u32,
    strings_at: u64,
    strings_size: u32,
}

impl SymtabLayout
{
    /// Parse the layout from a 64-bit little-endian Mach-O header and its load commands,
    /// as mapped at `load_address`.
    ///
    /// # Errors
    /// Returns an error for a malformed header, or one without `LC_SYMTAB`, `__TEXT` or
    /// `__LINKEDIT`.
    pub(crate) fn from_header(header: &[u8], load_address: u64) -> Result<Self>
    {
        let invalid = |error: object::Error| DebuggerError::InvalidArgument(format!("invalid image header: {error}"));
        let missing = |what: &str| DebuggerError::InvalidArgument(format!("image header has no {what}"));
        let mach = MachHeader64::<Endianness>::parse(header, 0).map_err(invalid)?;
        let endian = mach.endian().map_err(invalid)?;
        let mut commands = mach.load_commands(endian, header, 0).map_err(invalid)?;
        let mut text = None;
        let mut linkedit = None;
        let mut symtab = None;
        // File addresses of executable sections, by their 1-based index across segments.
        let mut sections = Vec::new();
        let mut index: u32 = 0;
        while let Some(command) = commands.next().map_err(invalid)? {
            if let Some((segment, data)) = command.segment_64().map_err(invalid)? {
                match segment.name() {
                    b"__TEXT" => text = Some(segment.vmaddr(endian)),
                    b"__LINKEDIT" => linkedit = Some((segment.vmaddr(endian), segment.fileoff(endian))),
                    _ => {}
                }
                for section in segment.sections(endian, data).map_err(invalid)? {
                    index += 1;
                    let flags = section.flags(endian);
                    if flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS) != 0
                        && let Ok(index) = u8::try_from(index)
                    {
                        let start = section.addr(endian);
                        sections.push((index, start, start.saturating_add(section.size(endian))));
                    }
                }
            } else if let Some(command) = command.symtab().map_err(invalid)? {
                symtab = Some((
                    command.symoff.get(endian),
                    command.nsyms.get(endian),
                    command.stroff.get(endian),
                    command.strsize.get(endian),
                ));
            }
        }
        let text = text.ok_or_else(|| missing("__TEXT segment"))?;
        let (linkedit_address, linkedit_offset) = linkedit.ok_or_else(|| missing("__LINKEDIT segment"))?;
        let (symoff, nsyms, stroff, strsize) = symtab.ok_or_else(|| missing("LC_SYMTAB"))?;
        if nsyms > MAX_SYMBOLS {
            return Err(DebuggerError::InvalidArgument(format!(
                "symbol table of {nsyms} entries exceeds the size limit"
            )));
        }
        let slide = load_address.wrapping_sub(text);
        // `fileoff` to `vmaddr` within `__LINKEDIT`, then to the runtime address.
        let linkedit = |offset: u32| {
            linkedit_address
                .wrapping_add(u64::from(offset))
                .wrapping_sub(linkedit_offset)
                .wrapping_add(slide)
        };
        Ok(Self {
            sections: sections
                .into_iter()
                .map(|(index, start, end)| (index, start.wrapping_add(slide), end.wrapping_add(slide)))
                .collect(),
            slide,
            symbols_at: linkedit(symoff),
            symbol_count: nsyms,
            strings_at: linkedit(stroff),
            strings_size: strsize,
        })
    }

    /// Whether `address` lies in one of the image's executable sections.
    pub(crate) fn contains(&self, address: Address) -> bool
    {
        self.section_of(address.value()).is_some()
    }

    fn section_of(&self, address: u64) -> Option<&(u8, u64, u64)>
    {
        self.sections
            .iter()
            .find(|&&(_, start, end)| start <= address && address < end)
    }

    /// Where the `nlist` entries lie in the target and how many bytes to read.
    pub(crate) fn nlist_range(&self) -> (Address, usize)
    {
        // `symbol_count` is bounded by `MAX_SYMBOLS`, so this cannot overflow.
        (Address::new(self.symbols_at), self.symbol_count as usize * NLIST_SIZE)
    }

    /// Index the `nlist` entries read from [`nlist_range`](Self::nlist_range): defined
    /// (`N_SECT`), non-debugging symbols in executable sections, by runtime address.
    pub(crate) fn parse_symbols(&self, nlists: &[u8]) -> SymbolTable
    {
        let entries = pod::slice_from_bytes::<Nlist64<LittleEndian>>(nlists, nlists.len() / NLIST_SIZE)
            .map(|(entries, _)| entries)
            .unwrap_or_default();
        let mut symbols: Vec<(u64, u32)> = entries
            .iter()
            .filter(|nlist| nlist.n_type & N_STAB == 0 && nlist.n_type & N_TYPE == N_SECT)
            .filter(|nlist| self.sections.iter().any(|&(index, ..)| index == nlist.n_sect))
            .map(|nlist| {
                (
                    nlist.n_value.get(LittleEndian).wrapping_add(self.slide),
                    nlist.n_strx.get(LittleEndian),
                )
            })
            .filter(|&(_, strx)| strx != 0 && strx < self.strings_size)
            .collect();
        symbols.sort_unstable();
        symbols.dedup_by_key(|&mut (address, _)| address);
        SymbolTable { symbols }
    }

    /// Where the string at `strx` starts in the target, and at most how many bytes of the
    /// string table follow it.
    pub(crate) fn string_range(&self, strx: u32) -> Option<(Address, usize)>
    {
        let remaining = self.strings_size.checked_sub(strx).filter(|&left| left > 0)?;
        Some((
            Address::new(self.strings_at.wrapping_add(u64::from(strx))),
            remaining as usize,
        ))
    }

    /// Start of the executable section containing `address`.
    fn section_start(&self, address: u64) -> Option<u64>
    {
        self.section_of(address).map(|&(_, start, _)| start)
    }
}

/// An image's defined function symbols, sorted by runtime address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SymbolTable
{
    /// `(runtime address, string table index)`; one entry per address.
    symbols: Vec<(u64, u32)>,
}

impl SymbolTable
{
    /// The string table index of the nearest symbol at or before `address` in the same
    /// executable section, or `None` when `address` is outside the executable sections or
    /// no symbol precedes it there.
    pub(crate) fn lookup(&self, layout: &SymtabLayout, address: Address) -> Option<u32>
    {
        let address = address.value();
        let section_start = layout.section_start(address)?;
        let after = self.symbols.partition_point(|&(start, _)| start <= address);
        let &(start, strx) = self.symbols[..after].last()?;
        (start >= section_start).then_some(strx)
    }
}

/// The function name of a raw symbol table name: Mach-O prefixes C-level names with an
/// underscore (`_main`, `__ZN...`), which is not part of the function's name.
pub(crate) fn strip_symbol_prefix(raw: &str) -> &str
{
    raw.strip_prefix('_').unwrap_or(raw)
}

#[cfg(test)]
mod tests
{
    use object::macho::{LC_SEGMENT_64, LC_SYMTAB, MH_EXECUTE, MH_MAGIC_64};

    use super::*;

    fn push_u32(out: &mut Vec<u8>, value: u32)
    {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(out: &mut Vec<u8>, value: u64)
    {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn name16(name: &str) -> [u8; 16]
    {
        let mut bytes = [0; 16];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        bytes
    }

    /// A segment command with `sections` of `(name, addr, size, flags)`.
    fn segment(name: &str, vmaddr: u64, vmsize: u64, fileoff: u64, sections: &[(&str, u64, u64, u32)]) -> Vec<u8>
    {
        let mut out = Vec::new();
        push_u32(&mut out, LC_SEGMENT_64);
        push_u32(&mut out, u32::try_from(72 + 80 * sections.len()).unwrap());
        out.extend_from_slice(&name16(name));
        push_u64(&mut out, vmaddr);
        push_u64(&mut out, vmsize);
        push_u64(&mut out, fileoff);
        push_u64(&mut out, vmsize);
        push_u32(&mut out, 7);
        push_u32(&mut out, 5);
        push_u32(&mut out, u32::try_from(sections.len()).unwrap());
        push_u32(&mut out, 0);
        for &(sectname, addr, size, flags) in sections {
            out.extend_from_slice(&name16(sectname));
            out.extend_from_slice(&name16(name));
            push_u64(&mut out, addr);
            push_u64(&mut out, size);
            out.extend_from_slice(&[0; 4 * 4]);
            push_u32(&mut out, flags);
            out.extend_from_slice(&[0; 4 * 3]);
        }
        out
    }

    /// `__TEXT` at file offset 0 with `__text` (code) and `__const` (data), `__LINKEDIT`
    /// at file offset `0x8000` mapped at `0x1_0000_8000`, and a symbol table at file
    /// offset `0x8100` with strings at `0x8200`.
    fn header() -> Vec<u8>
    {
        let mut commands = segment(
            "__TEXT",
            0x1_0000_0000,
            0x4000,
            0,
            &[
                ("__text", 0x1_0000_1000, 0x1000, S_ATTR_PURE_INSTRUCTIONS),
                ("__const", 0x1_0000_2000, 0x100, 0),
            ],
        );
        commands.extend(segment("__LINKEDIT", 0x1_0000_8000, 0x4000, 0x8000, &[]));
        push_u32(&mut commands, LC_SYMTAB);
        push_u32(&mut commands, 24);
        push_u32(&mut commands, 0x8100);
        push_u32(&mut commands, 5);
        push_u32(&mut commands, 0x8200);
        push_u32(&mut commands, 0x40);
        let mut out = Vec::new();
        push_u32(&mut out, MH_MAGIC_64);
        push_u32(&mut out, 0x0100_000c);
        push_u32(&mut out, 0);
        push_u32(&mut out, MH_EXECUTE);
        push_u32(&mut out, 3);
        push_u32(&mut out, u32::try_from(commands.len()).unwrap());
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        out.extend(commands);
        out
    }

    fn nlist(strx: u32, n_type: u8, n_sect: u8, value: u64) -> Vec<u8>
    {
        let mut out = Vec::new();
        push_u32(&mut out, strx);
        out.push(n_type);
        out.push(n_sect);
        out.extend_from_slice(&[0, 0]);
        push_u64(&mut out, value);
        out
    }

    #[test]
    fn layout_follows_the_slide_into_linkedit()
    {
        let layout = SymtabLayout::from_header(&header(), 0x1_0010_0000).expect("layout");
        assert_eq!(layout.nlist_range(), (Address::new(0x1_0010_8100), 5 * NLIST_SIZE));
        assert_eq!(layout.string_range(4), Some((Address::new(0x1_0010_8204), 0x3c)));
        assert_eq!(layout.string_range(0x40), None);
        assert!(layout.contains(Address::new(0x1_0010_1000)));
        assert!(!layout.contains(Address::new(0x1_0010_2000)), "data is not code");
        assert!(!layout.contains(Address::new(0x1_0000_1000)), "the unslid address");
    }

    #[test]
    fn lookup_takes_the_nearest_preceding_code_symbol_in_the_same_section()
    {
        let layout = SymtabLayout::from_header(&header(), 0x1_0000_0000).expect("layout");
        let nlists = [
            nlist(1, N_SECT | 1, 1, 0x1_0000_1000),
            nlist(2, N_SECT, 1, 0x1_0000_1100),
            // A debugging (stab) entry, a data symbol and an undefined symbol never name code.
            nlist(3, 0x24, 1, 0x1_0000_1180),
            nlist(4, N_SECT, 2, 0x1_0000_2000),
            nlist(5, 1, 0, 0),
        ]
        .concat();
        let table = layout.parse_symbols(&nlists);
        let at = |address| table.lookup(&layout, Address::new(address));
        assert_eq!(at(0x1_0000_1000), Some(1));
        assert_eq!(at(0x1_0000_10fc), Some(1));
        assert_eq!(at(0x1_0000_1100), Some(2));
        assert_eq!(at(0x1_0000_1ffc), Some(2));
        assert_eq!(at(0x1_0000_2000), None, "data");
        assert_eq!(at(0x1_0000_0ffc), None, "before the section");
    }

    #[test]
    fn mach_o_underscore_is_stripped_once()
    {
        assert_eq!(strip_symbol_prefix("_main"), "main");
        assert_eq!(strip_symbol_prefix("__pthread_start"), "_pthread_start");
        assert_eq!(strip_symbol_prefix("start"), "start");
    }
}
