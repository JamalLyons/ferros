//! Lookup in a Mach-O `__TEXT,__unwind_info` section (Apple compact unwind).
//!
//! The section maps every function of an image to a 32-bit encoding through a two-level
//! index (`<mach-o/compact_unwind_encoding.h>`):
//!
//! - a header: version (1), then offset and count of the common-encodings array, the
//!   personality array and the first-level index;
//! - first-level index entries of 12 bytes (`functionOffset`, `secondLevelPagesSectionOffset`,
//!   `lsdaIndexArraySectionOffset`), sorted by function offset; the last one is a sentinel
//!   whose function offset is the end of the covered range;
//! - second-level pages, either *regular* (kind 2: `entryPageOffset: u16`, `entryCount: u16`,
//!   then `(functionOffset: u32, encoding: u32)` pairs) or *compressed* (kind 3: also
//!   `encodingsPageOffset: u16` and `encodingsCount: u16`; each entry is a `u32` whose low
//!   24 bits are the function offset relative to the first-level entry and whose high 8
//!   bits index the common encodings, then the page's own encodings).
//!
//! Function offsets are relative to the image's Mach-O header, so a runtime pc maps to
//! `pc - load_address`.
//!
//! Only lookup is implemented. Interpreting an encoding is architecture-specific and is
//! done by the unwinder; [`Arm64Mode`] decodes the arm64 modes it uses.

/// Encoding found for an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactEntry
{
    /// Offset of the function's first instruction from the image header.
    pub(crate) function_offset: u32,
    /// The raw 32-bit encoding (0 means no unwind information).
    pub(crate) encoding: u32,
}

const REGULAR_PAGE: u32 = 2;
const COMPRESSED_PAGE: u32 = 3;

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16>
{
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes(slice.try_into().ok()?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32>
{
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn usize_at(bytes: &[u8], offset: usize) -> Option<usize>
{
    u32_at(bytes, offset).and_then(|value| usize::try_from(value).ok())
}

/// Index of the last element in `0..count` whose key is `<= target`, by binary search
/// over a sorted key function.
fn last_at_or_below(count: usize, target: u32, key: impl Fn(usize) -> Option<u32>) -> Option<usize>
{
    let (mut low, mut high) = (0usize, count);
    while low < high {
        let mid = low + (high - low) / 2;
        if key(mid)? <= target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low.checked_sub(1)
}

/// The entry covering `offset` (relative to the image header) in `section`, or `None`
/// when the section is malformed or does not cover it.
pub(crate) fn lookup(section: &[u8], offset: u32) -> Option<CompactEntry>
{
    if u32_at(section, 0)? != 1 {
        return None;
    }
    let common_offset = usize_at(section, 4)?;
    let common_count = usize_at(section, 8)?;
    let index_offset = usize_at(section, 20)?;
    let index_count = usize_at(section, 24)?;
    // The last first-level entry is the sentinel and covers nothing.
    let pages = index_count.checked_sub(1)?;
    let index_entry = |i: usize| index_offset.checked_add(i.checked_mul(12)?);
    let sentinel = u32_at(section, index_entry(pages)?)?;
    if offset >= sentinel {
        return None;
    }
    let first = last_at_or_below(pages, offset, |i| u32_at(section, index_entry(i)?))?;
    let first_entry = index_entry(first)?;
    let first_function = u32_at(section, first_entry)?;
    let page = usize_at(section, first_entry + 4)?;
    if page == 0 {
        return None;
    }
    let entries_offset = page.checked_add(usize::from(u16_at(section, page + 4)?))?;
    let entry_count = usize::from(u16_at(section, page + 6)?);
    match u32_at(section, page)? {
        REGULAR_PAGE => {
            let at = |i: usize| entries_offset.checked_add(i.checked_mul(8)?);
            let i = last_at_or_below(entry_count, offset, |i| u32_at(section, at(i)?))?;
            Some(CompactEntry {
                function_offset: u32_at(section, at(i)?)?,
                encoding: u32_at(section, at(i)? + 4)?,
            })
        }
        COMPRESSED_PAGE => {
            let encodings_offset = page.checked_add(usize::from(u16_at(section, page + 8)?))?;
            let encodings_count = usize::from(u16_at(section, page + 10)?);
            let at = |i: usize| entries_offset.checked_add(i.checked_mul(4)?);
            let function = |i: usize| {
                let raw = u32_at(section, at(i)?)?;
                first_function.checked_add(raw & 0x00FF_FFFF)
            };
            let i = last_at_or_below(entry_count, offset, function)?;
            let raw = u32_at(section, at(i)?)?;
            let encoding_index = usize::try_from(raw >> 24).ok()?;
            let encoding = if encoding_index < common_count {
                u32_at(section, common_offset.checked_add(encoding_index * 4)?)?
            } else {
                let local = encoding_index - common_count;
                if local >= encodings_count {
                    return None;
                }
                u32_at(section, encodings_offset.checked_add(local * 4)?)?
            };
            Some(CompactEntry {
                function_offset: function(i)?,
                encoding,
            })
        }
        _ => None,
    }
}

/// The arm64 unwind mode of an encoding (`UNWIND_ARM64_MODE_MASK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arm64Mode
{
    /// No compact information (encoding 0, or an unknown mode).
    None,
    /// `UNWIND_ARM64_MODE_FRAMELESS`: a leaf; the return address is in `x30` and the
    /// function allocated `stack_size` bytes below the caller's `sp`.
    Frameless
    {
        /// Bytes allocated on the stack.
        stack_size: u64,
    },
    /// `UNWIND_ARM64_MODE_DWARF`: the function's FDE is at this offset in `__eh_frame`.
    Dwarf
    {
        /// Offset of the FDE in `__eh_frame`.
        fde_offset: usize,
    },
    /// `UNWIND_ARM64_MODE_FRAME`: a standard frame record at `x29`.
    Frame,
}

impl Arm64Mode
{
    /// Decode the mode bits of `encoding`.
    pub(crate) fn decode(encoding: u32) -> Self
    {
        match encoding & 0x0F00_0000 {
            0x0200_0000 => Self::Frameless {
                stack_size: u64::from((encoding >> 12) & 0xFFF) * 16,
            },
            0x0300_0000 => Self::Dwarf {
                fde_offset: (encoding & 0x00FF_FFFF) as usize,
            },
            0x0400_0000 => Self::Frame,
            _ => Self::None,
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn push(bytes: &mut Vec<u8>, value: u32)
    {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push16(bytes: &mut Vec<u8>, value: u16)
    {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// One compressed page for functions at 0x1000 and 0x1040 (common encodings 0 and the
    /// page's own encoding), one regular page for 0x2000, and a sentinel at 0x3000.
    fn section() -> Vec<u8>
    {
        let mut s = Vec::new();
        // Header: version, common encodings (offset 28, count 1), personalities (0, 0),
        // index (offset 32, count 3).
        for value in [1, 28, 1, 0, 0, 32, 3] {
            push(&mut s, value);
        }
        push(&mut s, 0x0400_0000); // common encoding 0: frame
        assert_eq!(s.len(), 32);
        // Index: [0x1000 -> page at 68], [0x2000 -> page at 92], sentinel 0x3000.
        for (function, page) in [(0x1000, 68), (0x2000, 92), (0x3000, 0)] {
            push(&mut s, function);
            push(&mut s, page);
            push(&mut s, 0);
        }
        assert_eq!(s.len(), 68);
        // Compressed page: entries at +12, encodings at +20 (one).
        push(&mut s, COMPRESSED_PAGE);
        push16(&mut s, 12);
        push16(&mut s, 2);
        push16(&mut s, 20);
        push16(&mut s, 1);
        push(&mut s, 0); // 0x1000, common encoding 0
        push(&mut s, (1 << 24) | 0x40); // 0x1040, page encoding 0
        push(&mut s, 0x0200_1000); // frameless, 16 * 1 bytes
        assert_eq!(s.len(), 92);
        // Regular page.
        push(&mut s, REGULAR_PAGE);
        push16(&mut s, 8);
        push16(&mut s, 1);
        push(&mut s, 0x2000);
        push(&mut s, 0x0300_0158);
        s
    }

    #[test]
    fn lookup_reads_compressed_and_regular_pages()
    {
        let s = section();
        let entry = |offset| lookup(&s, offset).map(|e| (e.function_offset, e.encoding));
        assert_eq!(entry(0x0fff), None);
        assert_eq!(entry(0x1000), Some((0x1000, 0x0400_0000)));
        assert_eq!(entry(0x103c), Some((0x1000, 0x0400_0000)));
        assert_eq!(entry(0x1044), Some((0x1040, 0x0200_1000)));
        assert_eq!(entry(0x2abc), Some((0x2000, 0x0300_0158)));
        assert_eq!(entry(0x3000), None);
        assert_eq!(lookup(&s[..40], 0x1000), None, "truncated sections are rejected");
    }

    #[test]
    fn arm64_modes_decode()
    {
        assert_eq!(Arm64Mode::decode(0), Arm64Mode::None);
        assert_eq!(Arm64Mode::decode(0x0200_1000), Arm64Mode::Frameless { stack_size: 16 });
        assert_eq!(Arm64Mode::decode(0x0300_0158), Arm64Mode::Dwarf { fde_offset: 0x158 });
        // Personality and LSDA bits do not change the mode.
        assert_eq!(Arm64Mode::decode(0x5300_0158), Arm64Mode::Dwarf { fde_offset: 0x158 });
        assert_eq!(Arm64Mode::decode(0x0400_0000), Arm64Mode::Frame);
    }
}
