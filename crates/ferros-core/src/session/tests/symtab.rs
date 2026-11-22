//! Frames named from a mapped image's symbol table, on the mock backend.

use super::*;
use crate::types::{StackFrame, SymbolSource};

/// A 64-bit Mach-O image for the mapped layout: `__TEXT` (vmaddr 0x1_0000_0000, file
/// offset 0, 0x4000 bytes) with `__text` code at 0x1000..0x1100, `__LINKEDIT` at file
/// offset 0x4000 (0x2000 bytes), one symbol `_foo` at `__text`'s start in an `nlist` at
/// file offset 0x4000, and the string table at file offset 0x5000.
///
/// Returns the header page, the `nlist` page and the string page, each as mapped bytes.
fn image() -> (Vec<u8>, Vec<u8>, Vec<u8>)
{
    fn u32s(out: &mut Vec<u8>, values: &[u32])
    {
        for value in values {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn u64s(out: &mut Vec<u8>, values: &[u64])
    {
        for value in values {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn name(out: &mut Vec<u8>, name: &str)
    {
        let mut bytes = [0; 16];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&bytes);
    }
    let mut commands = Vec::new();
    // __TEXT with one section.
    u32s(&mut commands, &[0x19, 72 + 80]);
    name(&mut commands, "__TEXT");
    u64s(&mut commands, &[0x1_0000_0000, 0x4000, 0, 0x4000]);
    u32s(&mut commands, &[5, 5, 1, 0]);
    name(&mut commands, "__text");
    name(&mut commands, "__TEXT");
    u64s(&mut commands, &[0x1_0000_1000, 0x100]);
    u32s(&mut commands, &[0x1000, 2, 0, 0, 0x8000_0400, 0, 0, 0]);
    // __LINKEDIT.
    u32s(&mut commands, &[0x19, 72]);
    name(&mut commands, "__LINKEDIT");
    u64s(&mut commands, &[0x1_0000_4000, 0x2000, 0x4000, 0x2000]);
    u32s(&mut commands, &[1, 1, 0, 0]);
    // LC_SYMTAB.
    u32s(&mut commands, &[0x2, 24, 0x4000, 1, 0x5000, 0x10]);
    let mut header = Vec::new();
    u32s(
        &mut header,
        &[
            0xfeed_facf,
            0x0100_000c,
            0,
            2,
            3,
            u32::try_from(commands.len()).unwrap(),
            0,
            0,
        ],
    );
    header.extend(commands);
    header.resize(0x4000, 0);

    let mut nlists = Vec::new();
    u32s(&mut nlists, &[1]);
    nlists.extend_from_slice(&[0x0f, 1, 0, 0]);
    u64s(&mut nlists, &[0x1_0000_1000]);
    nlists.resize(0x1000, 0);

    let mut strings = b"\0_foo\0".to_vec();
    strings.resize(0x1000, 0);
    (header, nlists, strings)
}

fn image_at(path: &str, address: u64) -> LoadedImage
{
    LoadedImage {
        path: format!("/nonexistent/{path}").into(),
        load_address: Address::new(address),
        uuid: None,
    }
}

fn symbol(frame: &StackFrame) -> Option<(&str, SymbolSource)>
{
    frame.symbol.as_ref().map(|symbol| (symbol.display_name(), symbol.source()))
}

#[test]
fn failed_symbol_table_reads_are_retried_by_a_later_trace()
{
    // Issue #104 review: a failed `nlist` or string read leaves the frame unnamed for
    // that trace only; a later trace reads again and names it. The image is mapped
    // without execute permission so the unlisted-header search cannot find it: the name
    // must come from the listed image's own symbol table state.
    const IMAGE: u64 = 0x10_0000;
    let (header, nlists, strings) = image();
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r--", header)
        .with_image(image_at("ferros-symtab-image", IMAGE));
    let mut session = launched(&mock);

    // Neither the `nlist` entries nor the strings are mapped.
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), None);

    // The `nlist` entries are, but the strings are not.
    let _ = mock.clone().with_region(Address::new(IMAGE + 0x4000), "r--", nlists);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), None);

    let _ = mock.clone().with_region(Address::new(IMAGE + 0x5000), "r--", strings);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("foo", SymbolSource::SymbolTable)));
}

#[test]
fn header_search_skips_a_false_magic_word_before_the_real_header()
{
    // Issue #104 review: an `MH_MAGIC_64` word that is not a usable header (here at the
    // start of the pc's page, in code) does not end the search for the image's header.
    const LISTED: u64 = 0x10_0000;
    const UNLISTED: u64 = 0x40_0000;
    let (listed, ..) = image();
    let (mut header, nlists, strings) = image();
    header[0x1000..0x1004].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(UNLISTED + 0x1010))
        .with_region(Address::new(LISTED), "r-x", listed)
        .with_region(Address::new(UNLISTED), "r-x", header)
        .with_region(Address::new(UNLISTED + 0x4000), "r--", nlists)
        .with_region(Address::new(UNLISTED + 0x5000), "r--", strings)
        .with_image(image_at("ferros-symtab-listed", LISTED));
    let mut session = launched(&mock);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("foo", SymbolSource::SymbolTable)));
}

#[test]
fn a_failed_header_search_does_not_hide_a_later_frames_image()
{
    // Issue #104 re-review: a frame below an unlisted image's header finds nothing; a
    // later frame inside that image, in the same region, must still find its header.
    const LISTED: u64 = 0x10_0000;
    const CODE: u64 = 0x40_0000;
    let (listed, ..) = image();
    let (header, nlists, strings) = image();
    let mut region = vec![0u8; 0x1000];
    region.extend(header);
    let mut registers = regs_at(CODE + 0x10);
    registers.fp = Address::new(0x2000);
    registers.sp = Address::new(0x2000);
    let mut stack = vec![0u8; 16];
    stack[8..].copy_from_slice(&(CODE + 0x1000 + 0x1014).to_le_bytes());
    let mock = MockBackend::new()
        .with_thread(T1, registers)
        .with_region(Address::new(LISTED), "r-x", listed)
        .with_region(Address::new(CODE), "r-x", region)
        .with_region(Address::new(CODE + 0x5000), "r--", nlists)
        .with_region(Address::new(CODE + 0x6000), "r--", strings)
        .with_region(Address::new(0x2000), "rw-", stack)
        .with_image(image_at("ferros-symtab-listed", LISTED));
    let mut session = launched(&mock);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(frames[0].pc.value(), CODE + 0x10, "{frames:?}");
    assert_eq!(symbol(&frames[0]), None, "below the header: {frames:?}");
    assert_eq!(frames[1].pc.value(), CODE + 0x2014, "{frames:?}");
    assert_eq!(symbol(&frames[1]), Some(("foo", SymbolSource::SymbolTable)), "{frames:?}");
}

#[test]
fn header_search_runs_once_per_region_in_a_trace()
{
    // Two frames in a code region without any Mach-O header: the region is searched
    // for the first and remembered as headerless for the second.
    const LISTED: u64 = 0x10_0000;
    const CODE: u64 = 0x40_0000;
    let (listed, ..) = image();
    let mut registers = regs_at(CODE + 0x3010);
    registers.fp = Address::new(0x2000);
    registers.sp = Address::new(0x2000);
    let mut stack = vec![0u8; 16];
    stack[8..].copy_from_slice(&(CODE + 0x2014).to_le_bytes());
    let mock = MockBackend::new()
        .with_thread(T1, registers)
        .with_region(Address::new(LISTED), "r-x", listed)
        .with_region(Address::new(CODE), "r-x", vec![0; 0x4000])
        .with_region(Address::new(0x2000), "rw-", stack)
        .with_image(image_at("ferros-symtab-listed", LISTED));
    let mut session = launched(&mock);
    let frames = session.stack_trace(4).expect("stack trace");
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(pcs[..2], [CODE + 0x3010, CODE + 0x2014], "{frames:?}");
    assert!(frames.iter().all(|frame| frame.symbol.is_none()), "{frames:?}");
    let probes = mock
        .calls()
        .iter()
        .filter(|call| {
            matches!(call, MockCall::ReadMemory { address, len: 4 }
                if (CODE..CODE + 0x4000).contains(&address.value()) && address.value() % 0x1000 == 0)
        })
        .count();
    assert_eq!(probes, 4, "one probe per page, for the first frame only");
}

#[test]
fn a_listed_image_is_named_without_a_header_search()
{
    // Issue #121: dyld is now a listed image, so its code (the base of a main-thread
    // trace) is named from its own symbol table. The pc's region is executable, where a
    // search would probe each 4 KiB page below the pc for `MH_MAGIC_64`; for a listed
    // image no such probe is read.
    const IMAGE: u64 = 0x40_0000;
    let (header, nlists, strings) = image();
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r-x", header)
        .with_region(Address::new(IMAGE + 0x4000), "r--", nlists)
        .with_region(Address::new(IMAGE + 0x5000), "r--", strings)
        .with_image(image_at("dyld", IMAGE));
    let mut session = launched(&mock);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("foo", SymbolSource::SymbolTable)));
    let probes: Vec<_> = mock
        .calls()
        .into_iter()
        .filter(|call| matches!(call, MockCall::ReadMemory { len: 4, .. }))
        .collect();
    assert_eq!(probes, [], "no header probe for a listed image");
}

#[test]
fn a_replaced_image_at_the_same_address_is_reread_only_when_its_uuid_changes()
{
    // Issue #121: path and load address do not identify a build. An image unloaded and
    // replaced by a different one at the same place has a new UUID, and the names cached
    // from the old symbol table must not survive. The same UUID, or both unknown, is the
    // same image: a name already read is kept even when the mapped bytes change, because
    // those bytes do not change while that image stays loaded. The UUID is the identity.
    const IMAGE: u64 = 0x10_0000;
    let (header, nlists, strings) = image();
    let path = "/nonexistent/ferros-symtab-replaced";
    let listed = |uuid| LoadedImage {
        path: path.into(),
        load_address: Address::new(IMAGE),
        uuid,
    };
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r--", header)
        .with_region(Address::new(IMAGE + 0x4000), "r--", nlists)
        .with_region(Address::new(IMAGE + 0x5000), "r--", strings)
        .with_image(listed(None));
    let mut session = launched(&mock);

    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("foo", SymbolSource::SymbolTable)));

    // Both UUIDs unknown, and the string table now says `_bar`: the cached name is kept.
    mock.overwrite_memory(Address::new(IMAGE + 0x5000), b"\0_bar\0");
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("foo", SymbolSource::SymbolTable)));

    // A UUID where there was none is a different image, so the new string is read.
    mock.set_images(vec![listed(Some([1; 16]))]);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("bar", SymbolSource::SymbolTable)));

    // The same UUID again, with the string table now saying `_baz`: the name is kept.
    mock.overwrite_memory(Address::new(IMAGE + 0x5000), b"\0_baz\0");
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("bar", SymbolSource::SymbolTable)));

    // A different UUID at the same path and address reads the replacement's name.
    mock.set_images(vec![listed(Some([2; 16]))]);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("baz", SymbolSource::SymbolTable)));
}
