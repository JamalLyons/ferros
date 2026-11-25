//! Image lists: which library is the target, and how often images are enumerated.

use ferros_core::backend::LaunchSpec;
use ferros_core::backend::mock::{MockBackend, MockCall, arm64_registers};
use ferros_core::objfile::mock::{MockObjectFormat, MockObjectImage};
use ferros_core::runtime::mock::MockRuntime;
use ferros_core::types::Address;
use ferros_core::{BreakpointLocation, Session};

use crate::helpers::*;

/// A synthetic image in the neutral `MockObjectFormat` mapped at `base` (no slide), whose
/// DWARF holds one four-byte subprogram per `(name, address)`. Returns the file to remove
/// and the mapped bytes.
fn named_functions_image(prefix: &str, base: Address, functions: &[(&str, Address)]) -> (std::path::PathBuf, Vec<u8>)
{
    use ferros_core::objfile::SectionId;
    use ferros_core::objfile::mock::MockObjectFile;

    let (mut image, path) = ferros_tests::scratch::create_file(prefix, "");
    // DWARF 4, as in `MultiSiteImage`: subprograms with name, low_pc and a data4 high_pc.
    let abbrev = vec![1, 0x11, 1, 0, 0, 2, 0x2e, 0, 3, 8, 0x11, 1, 0x12, 6, 0, 0, 0];
    let mut info = vec![0; 4];
    info.extend_from_slice(&[4, 0, 0, 0, 0, 0, 8, 1]);
    for (name, address) in functions {
        info.push(2);
        info.extend_from_slice(name.as_bytes());
        info.push(0);
        info.extend_from_slice(&address.value().to_le_bytes());
        info.extend_from_slice(&4_u32.to_le_bytes());
    }
    info.push(0);
    let length = u32::try_from(info.len() - 4).unwrap();
    info[..4].copy_from_slice(&length.to_le_bytes());
    let file = MockObjectFile::new(base.value(), 0x2000)
        .with_dwarf(SectionId::DebugInfo, &info)
        .with_dwarf(SectionId::DebugAbbrev, &abbrev)
        .with_dwarf(SectionId::DebugLine, &[0]);
    std::io::Write::write_all(&mut image, &file.to_bytes()).expect("write synthetic image");
    drop(image);
    let mut bytes = MockObjectImage::new(0..1, 0, 0)
        .with_mapping(base.value(), 0x2000)
        .header_bytes();
    bytes.resize(0x2000, 0x1F);
    (path, bytes)
}

#[test]
fn library_listed_first_is_not_the_breakpoint_target()
{
    // Issue #220: the loader lists a library ahead of the program. File and function
    // breakpoints resolve in the image the backend marks as the main executable, wherever
    // it is listed, and a list that marks none does not fall back to a library.
    use ferros_core::DebuggerError;
    use ferros_core::backend::{ImageRole, LoadedImage};

    const LIBRARY: Address = Address::new(0x2_0000_0000);
    const EXECUTABLE: Address = Address::new(0x3_0000_0000);
    let (library_path, library) = named_functions_image("ferros-mock-library", LIBRARY, &[("lib_only", LIBRARY + 0x1000)]);
    let (executable_path, executable) =
        named_functions_image("ferros-mock-executable", EXECUTABLE, &[("main_only", EXECUTABLE + 0x1000)]);
    let _files = RemoveOnDrop(vec![library_path.clone(), executable_path.clone()]);
    let listed = |path: &std::path::Path, load_address: Address, role: ImageRole| LoadedImage {
        path: path.to_path_buf(),
        load_address,
        build_id: ferros_core::types::BuildIdentity::Unknown,
        role,
    };
    let mut registers = arm64_registers();
    registers.pc = EXECUTABLE + 0x1000;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(LIBRARY, "r-x", library)
        .with_region(EXECUTABLE, "r-x", executable)
        .with_image(listed(&library_path, LIBRARY, ImageRole::Library))
        .with_image(listed(&executable_path, EXECUTABLE, ImageRole::MainExecutable));
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    let id = session
        .set_breakpoint(BreakpointLocation::Function("main_only".into()))
        .expect("main_only resolves in the executable listed second");
    let sites = &session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .expect("the logical breakpoint")
        .sites;
    assert_eq!(sites.len(), 1);
    let address = sites[0].address;
    assert!(
        (EXECUTABLE.value()..EXECUTABLE.value() + 0x2000).contains(&address.value()),
        "site {address} is outside the executable's mapping"
    );

    // Libraries only: the library's own function is not a site, and the error is the
    // existing unresolved one.
    mock.set_images(vec![
        listed(&library_path, LIBRARY, ImageRole::Library),
        listed(&executable_path, EXECUTABLE, ImageRole::Library),
    ]);
    let locations = vec![
        BreakpointLocation::Function("lib_only".into()),
        BreakpointLocation::Function("main_only".into()),
        BreakpointLocation::Line {
            file: "main.rs".into(),
            line: 1,
        },
    ];
    for location in locations {
        match session.set_breakpoint(location.clone()) {
            Err(DebuggerError::InvalidArgument(_)) => {}
            other => panic!("{location} with no main executable: expected InvalidArgument, got {other:?}"),
        }
    }
}

#[test]
fn a_multi_site_breakpoint_enumerates_images_once()
{
    // Issue #220: resolving a location and naming each of its sites share one refreshed
    // image list, so N sites cost one loader enumeration, not N + 1.
    let (_image, mock) = MultiSiteImage::new();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let _ = mock.take_calls();
    let id = session
        .set_breakpoint(BreakpointLocation::Function("multi".into()))
        .expect("multi-site breakpoint");
    let breakpoint = session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .expect("the logical breakpoint");
    let sites = breakpoint.sites.clone();
    assert_eq!(sites.len(), 2);
    let enumerations = mock
        .take_calls()
        .iter()
        .filter(|call| matches!(call, MockCall::Images))
        .count();
    assert_eq!(enumerations, 1, "one image enumeration for two sites");
    // Positioning a site within the resolving request reports what a separate lookup does.
    for site in sites {
        assert_eq!(session.source_position(site.address).expect("source position"), site.position);
    }
}

/// #214: a signal that lands on a function's first instruction is named for that function.
/// The interrupted frame is not at a return address, so symbol-table naming looks up its
/// pc itself; a pc four bytes into the previous function would name the wrong one. An
/// ordinary caller whose return address is the same address is named from its call
/// instruction, which is the previous function's last.
#[test]
fn signal_at_function_entry_is_not_named_as_the_previous_function()
{
    use ferros_core::backend::{ImageRole, LoadedImage};
    use ferros_core::types::SymbolSource;

    const BASE: u64 = 0x5_0000_0000;
    const TRAMPOLINE: u64 = 0x6_0000_0000;
    const PREVIOUS: u64 = BASE + 0x1000;
    const ENTRY: u64 = BASE + 0x1080;
    const STACK_BASE: u64 = 0x1000;
    const HANDLER_FP: u64 = 0x1000;
    const TRAMPOLINE_FP: u64 = 0x1100;
    const CONTEXT: u64 = TRAMPOLINE_FP + MockRuntime::CONTEXT_OFFSET;
    const INTERRUPTED_SP: u64 = 0x2000;
    const INTERRUPTED_FP: u64 = 0x2100;

    let symbols = MockObjectImage::new(PREVIOUS..PREVIOUS + 0x100, BASE + 0x4000, BASE + 0x5000)
        .with_symbol(PREVIOUS, "previous_fn")
        .with_symbol(ENTRY, "entry_fn");

    let mut stack = vec![0u8; 0x2000];
    let mut put = |at: u64, value: u64| {
        let offset = usize::try_from(at - STACK_BASE).expect("offset");
        stack[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };
    // The handler's frame record returns into the trampoline.
    put(HANDLER_FP, TRAMPOLINE_FP);
    put(HANDLER_FP + 8, TRAMPOLINE + 0x44);
    put(TRAMPOLINE_FP, INTERRUPTED_FP);
    put(TRAMPOLINE_FP + 8, PREVIOUS + 0x10);
    // The interrupted frame is on the first instruction of `entry_fn`.
    for index in 0..MockRuntime::GENERAL_WORDS {
        put(CONTEXT + 8 * index, 0x100 + index);
    }
    put(CONTEXT + MockRuntime::FP, INTERRUPTED_FP);
    put(CONTEXT + MockRuntime::LR, PREVIOUS + 0x10);
    put(CONTEXT + MockRuntime::SP, INTERRUPTED_SP);
    put(CONTEXT + MockRuntime::PC, ENTRY);
    // Its caller's return address is also `ENTRY`: the call was the last instruction of
    // `previous_fn`.
    put(INTERRUPTED_FP, 0);
    put(INTERRUPTED_FP + 8, ENTRY);

    let mut registers = arm64_registers();
    registers.pc = Address::new(PREVIOUS + 0x40);
    registers.sp = Address::new(0xf00);
    registers.fp = Address::new(HANDLER_FP);
    registers.general[29] = HANDLER_FP;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(Address::new(BASE), "r-x", symbols.header_bytes())
        .with_region(Address::new(PREVIOUS), "r-x", vec![0x1F; 0x100])
        .with_region(Address::new(BASE + 0x4000), "r--", symbols.table_bytes())
        .with_region(Address::new(BASE + 0x5000), "r--", symbols.name_bytes())
        .with_region(Address::new(TRAMPOLINE), "r-x", vec![0x1F; 0x100])
        .with_region(Address::new(0), "rw-", vec![0; 0x1000])
        .with_region(Address::new(STACK_BASE), "rw-", stack)
        .with_image(LoadedImage {
            path: "/nonexistent/ferros-entry-image".into(),
            load_address: Address::new(BASE),
            build_id: ferros_core::BuildIdentity::Unknown,
            role: ImageRole::MainExecutable,
        });
    let runtime = MockRuntime::new()
        .with_signal_trampoline(TRAMPOLINE..TRAMPOLINE + 0x100)
        .into_shared();
    let mut session = Session::new(Box::new(mock), MockObjectFormat::shared()).with_runtime(runtime);
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    let frames = session.stack_trace(16).expect("stack trace");
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(pcs, [PREVIOUS + 0x40, TRAMPOLINE + 0x44, ENTRY, ENTRY], "{frames:#?}");
    let name = |index: usize| {
        let symbol = frames[index].symbol.as_ref();
        symbol.map(|symbol| {
            assert_eq!(symbol.source(), SymbolSource::SymbolTable);
            symbol.display_name().to_owned()
        })
    };
    assert_eq!(name(0).as_deref(), Some("previous_fn"));
    assert_eq!(
        name(2).as_deref(),
        Some("entry_fn"),
        "the interrupted frame is named from its pc"
    );
    assert_eq!(
        name(3).as_deref(),
        Some("previous_fn"),
        "an ordinary caller is named from its call instruction"
    );
}
