//! The Mach-O format through a [`Session`] on the scripted mock backend: frames named
//! from a mapped image's `LC_SYMTAB`, and the `MH_MAGIC_64` search for unlisted images.

use ferros_core::Session;
use ferros_core::backend::mock::{MockBackend, MockCall, arm64_registers};
use ferros_core::backend::{LaunchSpec, LoadedImage};
use ferros_core::types::{Address, BuildIdentity, Registers, StackFrame, SymbolSource, ThreadId};

use super::{MH_MAGIC_64, MachOFormat};

const T1: ThreadId = ThreadId(1);

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
            MH_MAGIC_64,
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

fn regs_at(pc: u64) -> Registers
{
    let mut registers = arm64_registers();
    registers.pc = Address::new(pc);
    registers
}

fn image_at(path: &str, address: u64) -> LoadedImage
{
    LoadedImage {
        path: format!("/nonexistent/{path}").into(),
        load_address: Address::new(address),
        build_id: BuildIdentity::Unknown,
        role: ferros_core::backend::ImageRole::MainExecutable,
    }
}

fn launched(mock: &MockBackend) -> Session
{
    let mut session = Session::new(Box::new(mock.clone()), MachOFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let _ = mock.take_calls();
    session
}

fn symbol(frame: &StackFrame) -> Option<(&str, SymbolSource)>
{
    frame.symbol.as_ref().map(|symbol| (symbol.display_name(), symbol.source()))
}

#[test]
fn failed_symbol_table_reads_are_retried_by_a_later_trace()
{
    // Issue #104 review: a failed `nlist` or string read leaves the frame unnamed for
    // that trace only; a later trace reads again and names it, without its underscore.
    const IMAGE: u64 = 0x10_0000;
    let (header, nlists, strings) = image();
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x1010))
        .with_region(Address::new(IMAGE), "r--", header)
        .with_image(image_at("ferros-symtab-image", IMAGE));
    let mut session = launched(&mock);
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(symbol(&frames[0]), None);

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
    header[0x1000..0x1004].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
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
fn header_search_probes_each_4k_page_once_per_trace()
{
    // Two frames in a code region without any Mach-O header: the region is searched at
    // 4 KiB steps for the first and remembered as headerless for the second.
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
    // Issue #121: a listed image (dyld) is named from its own symbol table; no page
    // below the pc is probed for `MH_MAGIC_64`.
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
