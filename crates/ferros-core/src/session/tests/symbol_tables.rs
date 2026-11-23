//! Frames named from a mapped image's symbol table, on the mock backend and the neutral
//! [`MockObjectFormat`].

use super::*;
use crate::objfile::mock::MockObjectImage;
use crate::types::{BuildId, StackFrame, SymbolSource};

/// An image in the neutral [`MockObjectFormat`], laid out from `base`: its header at
/// `base`, code at `base + 0x1000..base + 0x1100` with one symbol `foo` at its start, the
/// table at `base + 0x4000` and the names at `base + 0x5000`.
///
/// Returns the header page (0x4000 bytes, so the code lies in it), the table page and the
/// names page, each as mapped bytes.
fn image(base: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>)
{
    let image =
        MockObjectImage::new(base + 0x1000..base + 0x1100, base + 0x4000, base + 0x5000).with_symbol(base + 0x1000, "foo");
    let mut header = image.header_bytes();
    header.resize(0x4000, 0);
    let mut table = image.table_bytes();
    table.resize(0x1000, 0);
    let mut names = image.name_bytes();
    names.resize(0x1000, 0);
    (header, table, names)
}

fn image_at(path: &str, address: u64) -> LoadedImage
{
    LoadedImage {
        path: format!("/nonexistent/{path}").into(),
        load_address: Address::new(address),
        build_id: crate::types::BuildIdentity::Unknown,
    }
}

fn symbol(frame: &StackFrame) -> Option<(&str, SymbolSource)>
{
    frame.symbol.as_ref().map(|symbol| (symbol.display_name(), symbol.source()))
}

#[test]
fn failed_symbol_table_reads_are_retried_by_a_later_trace()
{
    // Issue #104 review: a failed table or name read leaves the frame unnamed for
    // that trace only; a later trace reads again and names it. The image is mapped
    // without execute permission so the unlisted-header search cannot find it: the name
    // must come from the listed image's own symbol table state.
    const IMAGE: u64 = 0x10_0000;
    let (header, table, names) = image(IMAGE);
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r--", header)
        .with_image(image_at("ferros-symtab-image", IMAGE));
    let mut session = launched(&mock);

    // Neither the table nor the names are mapped.
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), None);

    // The table is, but the names are not.
    let _ = mock.clone().with_region(Address::new(IMAGE + 0x4000), "r--", table);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), None);

    let _ = mock.clone().with_region(Address::new(IMAGE + 0x5000), "r--", names);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), Some(("foo", SymbolSource::SymbolTable)));
}

#[test]
fn header_search_skips_a_false_magic_word_before_the_real_header()
{
    // Issue #104 review: a magic word that is not a usable header (here at the start of
    // the pc's page, in code) does not end the search for the image's header.
    const LISTED: u64 = 0x10_0000;
    const UNLISTED: u64 = 0x40_0000;
    let (listed, ..) = image(LISTED);
    let (mut header, table, names) = image(UNLISTED);
    header[0x1000..0x1004].copy_from_slice(&MockObjectFormat::MAGIC);
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(UNLISTED + 0x1010))
        .with_region(Address::new(LISTED), "r-x", listed)
        .with_region(Address::new(UNLISTED), "r-x", header)
        .with_region(Address::new(UNLISTED + 0x4000), "r--", table)
        .with_region(Address::new(UNLISTED + 0x5000), "r--", names)
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
    let (listed, ..) = image(LISTED);
    let (header, table, names) = image(CODE + 0x1000);
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
        .with_region(Address::new(CODE + 0x5000), "r--", table)
        .with_region(Address::new(CODE + 0x6000), "r--", names)
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
    // Two frames in a code region without any image header: the region is searched for
    // the first and remembered as headerless for the second.
    const LISTED: u64 = 0x10_0000;
    const CODE: u64 = 0x40_0000;
    let (listed, ..) = image(LISTED);
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
    // search would probe each 4 KiB page below the pc for a header's magic word; for a
    // listed image no such probe is read.
    const IMAGE: u64 = 0x40_0000;
    let (header, table, names) = image(IMAGE);
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r-x", header)
        .with_region(Address::new(IMAGE + 0x4000), "r--", table)
        .with_region(Address::new(IMAGE + 0x5000), "r--", names)
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
fn a_replaced_image_at_the_same_address_is_reread_only_when_its_known_uuid_changes()
{
    // Issue #121: path and load address do not identify a build. An image unloaded and
    // replaced by a different one at the same place has a new UUID, and the names cached
    // from the old symbol table must not survive. An unknown UUID is no evidence of a
    // change: a backend may fail to read it at a stop. The same UUID, or an unknown one
    // on either side, preserves the cached name even when the mocked bytes change.
    // Only two known, different UUIDs trigger a re-read.
    const IMAGE: u64 = 0x10_0000;
    let (header, table, names) = image(IMAGE);
    let path = "/nonexistent/ferros-symtab-replaced";
    let listed = |uuid: Option<[u8; 16]>| LoadedImage {
        path: path.into(),
        load_address: Address::new(IMAGE),
        build_id: uuid.map(BuildId::from).into(),
    };
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r--", header)
        .with_region(Address::new(IMAGE + 0x4000), "r--", table)
        .with_region(Address::new(IMAGE + 0x5000), "r--", names)
        .with_image(listed(None));
    let mut session = launched(&mock);
    let name = |session: &mut Session| {
        let frames = session.stack_trace(4).expect("stack trace");
        symbol(&frames[0]).map(|(name, source)| (name.to_owned(), source))
    };
    let named = |name: &str| Some((name.to_owned(), SymbolSource::SymbolTable));

    assert_eq!(name(&mut session), named("foo"));

    // Both UUIDs unknown, and the names now say `bar`: the cached name is kept.
    mock.overwrite_memory(Address::new(IMAGE + 0x5000), b"bar");
    assert_eq!(name(&mut session), named("foo"));

    // The UUID becomes known: still the same image, so the name is kept and the UUID
    // is recorded.
    mock.set_images(vec![listed(Some([1; 16]))]);
    assert_eq!(name(&mut session), named("foo"));

    // An unknown UUID again (a failed read of dyld's UUID array) changes nothing either.
    mock.set_images(vec![listed(None)]);
    assert_eq!(name(&mut session), named("foo"));

    // A different known UUID at the same path and address is a replacement: its name is
    // read. Without the recorded UUID this would look like an unknown one and keep `foo`.
    mock.set_images(vec![listed(Some([2; 16]))]);
    assert_eq!(name(&mut session), named("bar"));

    // The same UUID again, with the names now saying `baz`: the name is kept.
    mock.overwrite_memory(Address::new(IMAGE + 0x5000), b"baz");
    assert_eq!(name(&mut session), named("bar"));

    // And another build replaces it: `baz`.
    mock.set_images(vec![listed(Some([3; 16]))]);
    assert_eq!(name(&mut session), named("baz"));
}

#[test]
fn runtime_address_refuses_an_image_whose_known_uuid_differs()
{
    // Issue #121: an image at the same path and address with a different known UUID is
    // a different build, so its file addresses do not apply. An unknown UUID on either
    // side is no evidence of a change.
    let listed = |uuid: Option<[u8; 16]>| LoadedImage {
        path: "/nonexistent/ferros-uuid-image".into(),
        load_address: Address::new(0x10_0000),
        build_id: uuid.map(BuildId::from).into(),
    };
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(0x10_1010))
        .with_region(Address::new(0x10_0000), "r-x", mapped_text_header())
        .with_image(listed(Some([1; 16])));
    let mut session = launched(&mock);
    let at = |session: &mut Session, uuid| {
        session
            .runtime_address(&listed(uuid), 0x1_0000_0800)
            .expect("runtime address")
    };
    assert_eq!(at(&mut session, Some([1; 16])), Some(Address::new(0x10_0800)));
    assert_eq!(at(&mut session, None), Some(Address::new(0x10_0800)));
    assert_eq!(at(&mut session, Some([2; 16])), None);
}
