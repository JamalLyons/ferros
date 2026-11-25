//! Line breakpoints over synthetic line tables.

use ferros_core::backend::LaunchSpec;
use ferros_core::backend::mock::{MockBackend, MockCall, arm64_registers};
use ferros_core::objfile::mock::{MockObjectFormat, MockObjectImage};
use ferros_core::types::Address;
use ferros_core::{BreakpointLocation, Session};

use crate::helpers::*;

// Issue #244: line breakpoints over synthetic line tables.

/// A synthetic image in the neutral `MockObjectFormat` mapped at `base` (no slide), whose
/// DWARF has one compile unit for `file` with a line table: each `(line, addresses)` is a
/// row per address, in one sequence per address so a line can have several disjoint
/// sites. Returns the file to remove and the mapped bytes.
fn line_table_image(prefix: &str, base: Address, file: &str, lines: &[(u64, &[Address])]) -> (std::path::PathBuf, Vec<u8>)
{
    use ferros_core::objfile::mock::MockObjectFile;
    use gimli::write::{Address as WriteAddress, AttributeValue, DwarfUnit, EndianVec, LineProgram, LineString, Sections};
    use gimli::{Encoding, Format, LineEncoding, RunTimeEndian};

    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };
    let mut program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(b"/src".to_vec()),
        None,
        LineString::String(file.as_bytes().to_vec()),
        None,
    );
    let directory = program.default_directory();
    let file_id = program.add_file(LineString::String(file.as_bytes().to_vec()), directory, None);
    for &(line, addresses) in lines {
        for address in addresses {
            program.begin_sequence(Some(WriteAddress::Constant(address.value())));
            let row = program.row();
            row.file = file_id;
            row.line = line;
            row.is_statement = true;
            program.generate_row();
            program.end_sequence(4);
        }
    }
    let mut unit = DwarfUnit::new(encoding);
    unit.unit.line_program = program;
    let root = unit.unit.root();
    let entry = unit.unit.get_mut(root);
    entry.set(gimli::DW_AT_name, AttributeValue::String(file.as_bytes().to_vec()));
    entry.set(gimli::DW_AT_comp_dir, AttributeValue::String(b"/src".to_vec()));
    // A function around each row, so each site is positioned in a frame.
    for (index, address) in lines.iter().flat_map(|(_, addresses)| addresses.iter()).enumerate() {
        let function = unit.unit.add(root, gimli::DW_TAG_subprogram);
        let entry = unit.unit.get_mut(function);
        entry.set(
            gimli::DW_AT_name,
            AttributeValue::String(format!("line_fn{index}").into_bytes()),
        );
        entry.set(
            gimli::DW_AT_low_pc,
            AttributeValue::Address(WriteAddress::Constant(address.value())),
        );
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(4));
    }
    let mut sections = Sections::new(EndianVec::new(RunTimeEndian::Little));
    unit.write(&mut sections).expect("write synthetic DWARF");
    let mut file_bytes = MockObjectFile::new(base.value(), 0x2000);
    sections
        .for_each(|id, data| {
            if !data.slice().is_empty() {
                file_bytes = std::mem::replace(&mut file_bytes, MockObjectFile::new(0, 0)).with_dwarf(id, data.slice());
            }
            Ok::<(), ()>(())
        })
        .expect("collect sections");

    let (mut image, path) = ferros_tests::scratch::create_file(prefix, "");
    std::io::Write::write_all(&mut image, &file_bytes.to_bytes()).expect("write synthetic image");
    drop(image);
    let mut bytes = MockObjectImage::new(0..1, 0, 0)
        .with_mapping(base.value(), 0x2000)
        .header_bytes();
    bytes.resize(0x2000, 0x1F);
    (path, bytes)
}

fn listed_image(
    path: &std::path::Path,
    load_address: Address,
    role: ferros_core::backend::ImageRole,
) -> ferros_core::backend::LoadedImage
{
    ferros_core::backend::LoadedImage {
        path: path.to_path_buf(),
        load_address,
        build_id: ferros_core::types::BuildIdentity::Unknown,
        role,
    }
}

/// Line 7 of `lines.rs` has two disjoint sites in the main executable.
const LINE_SITES_BASE: Address = Address::new(0x4_0000_0000);

const LINE_SITES: [Address; 2] = [Address::new(0x4_0000_1000), Address::new(0x4_0000_1100)];

#[test]
fn a_multi_site_line_breakpoint_enumerates_images_once()
{
    // Issue #244 (from #220): resolving a `file:line` location and positioning each of its
    // sites share one refreshed image list, as for function sites
    // (`a_multi_site_breakpoint_enumerates_images_once`).
    use ferros_core::backend::ImageRole;

    let (path, bytes) = line_table_image("ferros-mock-lines", LINE_SITES_BASE, "lines.rs", &[(7, &LINE_SITES)]);
    let _files = RemoveOnDrop(vec![path.clone()]);
    let mut registers = arm64_registers();
    registers.pc = LINE_SITES[0];
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(LINE_SITES_BASE, "r-x", bytes)
        .with_image(listed_image(&path, LINE_SITES_BASE, ImageRole::MainExecutable));
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let _ = mock.take_calls();

    let id = session
        .set_breakpoint(BreakpointLocation::Line {
            file: "lines.rs".into(),
            line: 7,
        })
        .expect("multi-site line breakpoint");
    let sites = session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .expect("the logical breakpoint")
        .sites
        .clone();
    assert_eq!(sites.iter().map(|site| site.address).collect::<Vec<_>>(), LINE_SITES.to_vec());
    for site in &sites {
        assert_eq!(site.position.line, Some(7), "{site:?}");
    }
    let enumerations = mock
        .take_calls()
        .iter()
        .filter(|call| matches!(call, MockCall::Images))
        .count();
    assert_eq!(enumerations, 1, "one image enumeration for two line sites");
}

#[test]
fn pending_line_breakpoint_resolves_in_the_main_executable_listed_after_a_library()
{
    // Issue #244 (from #220): a line breakpoint set with no process stays pending, and at
    // launch it resolves in the image marked as the main executable even when the loader
    // lists a library first, and the library has the same file and line.
    use ferros_core::backend::ImageRole;

    const LIBRARY: Address = Address::new(0x2_0000_0000);
    const EXECUTABLE: Address = Address::new(0x3_0000_0000);
    let (library_path, library) =
        line_table_image("ferros-mock-lines-library", LIBRARY, "main.rs", &[(3, &[LIBRARY + 0x1000])]);
    let (executable_path, executable) = line_table_image(
        "ferros-mock-lines-executable",
        EXECUTABLE,
        "main.rs",
        &[(3, &[EXECUTABLE + 0x1000])],
    );
    let _files = RemoveOnDrop(vec![library_path.clone(), executable_path.clone()]);
    let mut registers = arm64_registers();
    registers.pc = EXECUTABLE + 0x1000;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(LIBRARY, "r-x", library)
        .with_region(EXECUTABLE, "r-x", executable)
        .with_image(listed_image(&library_path, LIBRARY, ImageRole::Library))
        .with_image(listed_image(&executable_path, EXECUTABLE, ImageRole::MainExecutable));
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());

    let id = session
        .set_breakpoint(BreakpointLocation::Line {
            file: "main.rs".into(),
            line: 3,
        })
        .expect("pending line breakpoint");
    let breakpoint = |session: &Session| {
        session
            .logical_breakpoints()
            .iter()
            .find(|breakpoint| breakpoint.id == id)
            .expect("the logical breakpoint")
            .clone()
    };
    assert!(breakpoint(&session).pending.is_some(), "set with no process");

    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let resolved = breakpoint(&session);
    assert_eq!(resolved.pending, None, "resolved at launch");
    assert_eq!(
        resolved.sites.iter().map(|site| site.address).collect::<Vec<_>>(),
        vec![EXECUTABLE + 0x1000],
        "the site is in the executable, not the library listed first"
    );
}

#[test]
fn two_images_marked_main_executable_are_reported()
{
    // Issue #244 (from #220): a backend that marks two images as the main executable
    // breaks the `ImageRole` contract. Breakpoints are not resolved in whichever is listed
    // first: the location fails with an error naming both, and so does a pending one at
    // launch (it stays pending with that reason).
    use ferros_core::DebuggerError;
    use ferros_core::backend::ImageRole;

    const FIRST: Address = Address::new(0x2_0000_0000);
    const SECOND: Address = Address::new(0x3_0000_0000);
    let (first_path, first) = line_table_image("ferros-mock-two-main-a", FIRST, "main.rs", &[(3, &[FIRST + 0x1000])]);
    let (second_path, second) = line_table_image("ferros-mock-two-main-b", SECOND, "main.rs", &[(3, &[SECOND + 0x1000])]);
    let _files = RemoveOnDrop(vec![first_path.clone(), second_path.clone()]);
    let mut registers = arm64_registers();
    registers.pc = FIRST + 0x1000;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(FIRST, "r-x", first)
        .with_region(SECOND, "r-x", second)
        .with_image(listed_image(&first_path, FIRST, ImageRole::MainExecutable))
        .with_image(listed_image(&second_path, SECOND, ImageRole::MainExecutable));
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    let location = BreakpointLocation::Line {
        file: "main.rs".into(),
        line: 3,
    };
    let pending = session.set_breakpoint(location.clone()).expect("pending breakpoint");
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    let reason = session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == pending)
        .and_then(|breakpoint| breakpoint.pending.clone())
        .expect("still pending");
    assert!(reason.contains("more than one image"), "{reason}");
    match session.set_breakpoint(location) {
        Err(DebuggerError::InvalidArgument(message)) => {
            assert!(
                message.contains(&first_path.display().to_string()) && message.contains(&second_path.display().to_string()),
                "{message}"
            );
        }
        other => panic!("expected InvalidArgument naming both images, got {other:?}"),
    }
}

#[test]
fn stack_trace_and_symbolicate_enumerate_images_once_per_call()
{
    // Issue #244 (from #220): each public stack request uses one refreshed image list for
    // all of its frames, however many images it loads. The list is not kept between
    // calls (see `Session::stack_trace`), so each call enumerates once. The call chain
    // spans two images, so "once" cannot hold by a single image being the only one read.
    use ferros_core::backend::ImageRole;

    const LIBRARY_IMAGE: Address = Address::new(0x2_0000_0000);
    const EXE_IMAGE: Address = Address::new(0x3_0000_0000);
    const STACK_AT: Address = Address::new(0x7_0000_0000);
    // `line_table_image` names each row's function `line_fn<row index>`: the caller's
    // image has a row before its call site, so the two images name their frames apart.
    let library_pc = LIBRARY_IMAGE + 0x1000;
    let call_site = EXE_IMAGE + 0x1000;
    let (library_path, library_bytes) =
        line_table_image("ferros-mock-chain-library", LIBRARY_IMAGE, "callee.rs", &[(3, &[library_pc])]);
    let (exe_path, exe_bytes) = line_table_image(
        "ferros-mock-chain-exe",
        EXE_IMAGE,
        "caller.rs",
        &[(4, &[EXE_IMAGE + 0x800]), (5, &[call_site])],
    );
    let _files = RemoveOnDrop(vec![library_path.clone(), exe_path.clone()]);
    // The callee is stopped on its first instruction, so its caller is the link register: a
    // return address one instruction past the call in the caller's image. The frame record
    // at fp ends the chain (saved fp 0, saved lr 0).
    let stack = vec![0u8; 32];
    let mut registers = arm64_registers();
    registers.pc = library_pc;
    registers.fp = STACK_AT;
    registers.sp = STACK_AT;
    registers.general[30] = (call_site + 4).value();
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(LIBRARY_IMAGE, "r-x", library_bytes)
        .with_region(EXE_IMAGE, "r-x", exe_bytes)
        .with_region(STACK_AT, "rw-", stack)
        .with_image(listed_image(&library_path, LIBRARY_IMAGE, ImageRole::Library))
        .with_image(listed_image(&exe_path, EXE_IMAGE, ImageRole::MainExecutable));
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let enumerations = |mock: &MockBackend| {
        mock.take_calls()
            .iter()
            .filter(|call| matches!(call, MockCall::Images))
            .count()
    };
    let _ = enumerations(&mock);

    let frames = session.stack_trace(16).expect("stack trace");
    assert_eq!(enumerations(&mock), 1, "one image enumeration for a two-image stack trace");
    assert_eq!(frames.len(), 2, "{frames:#?}");
    let names: Vec<Option<&str>> = frames
        .iter()
        .map(|frame| frame.symbol.as_ref().map(ferros_core::SymbolName::display_name))
        .collect();
    assert_eq!(names, [Some("line_fn0"), Some("line_fn1")], "{frames:#?}");
    assert_eq!(frames[0].pc, library_pc);
    assert_eq!(frames[1].pc, call_site + 4);

    for (address, name) in [(library_pc, "line_fn0"), (call_site, "line_fn1")] {
        let found = session
            .symbolicate(address)
            .expect("symbolicate")
            .unwrap_or_else(|| panic!("no symbolication for {address}"));
        let innermost = found.frames.last().unwrap_or_else(|| panic!("no frames for {address}"));
        assert_eq!(innermost.symbol.display_name(), name, "{address}");
        assert_eq!(enumerations(&mock), 1, "one image enumeration for symbolicating {address}");
    }
}
