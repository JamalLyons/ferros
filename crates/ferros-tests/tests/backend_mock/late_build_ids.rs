//! Build ids that arrive late and contradict an open file.

use ferros_core::Session;
use ferros_core::backend::LaunchSpec;
use ferros_core::backend::mock::{MockBackend, arm64_registers};
use ferros_core::objfile::SavedRegisterSlots;
use ferros_core::objfile::mock::{MockObjectFormat, MockObjectImage};
use ferros_core::types::Address;

use crate::helpers::*;

/// Load address of the images in the late build id tests below (no slide).
const LATE_ID_IMAGE: Address = Address::new(0x6_0000_0000);

/// Where the late build id tests' images name their one function and hint it.
const LATE_ID_FUNCTION: Address = Address::new(0x6_0000_1000);

/// Executable memory outside the image, the link register of the late build id tests.
const LATE_ID_CALLER: Address = Address::new(0x7_0000_0000);

/// The file of an image at `LATE_ID_IMAGE` recording `build_id`, whose DWARF names one
/// four-byte function `name` at `LATE_ID_FUNCTION` and whose own unwind hint calls that
/// function a frameless leaf.
fn late_id_file(name: &str, build_id: &[u8]) -> Vec<u8>
{
    use ferros_core::objfile::mock::MockObjectFile;
    use ferros_core::objfile::{SectionId, UnwindHint};

    // DWARF 4: a compile unit covering the function (low_pc, data4 high_pc), so an address
    // lookup finds it, and one subprogram with name, low_pc and a data4 high_pc.
    let abbrev = vec![
        1, 0x11, 1, 0x11, 1, 0x12, 6, 0, 0, 2, 0x2e, 0, 3, 8, 0x11, 1, 0x12, 6, 0, 0, 0,
    ];
    let mut info = vec![0; 4];
    info.extend_from_slice(&[4, 0, 0, 0, 0, 0, 8, 1]);
    info.extend_from_slice(&LATE_ID_FUNCTION.value().to_le_bytes());
    info.extend_from_slice(&4_u32.to_le_bytes());
    info.push(2);
    info.extend_from_slice(name.as_bytes());
    info.push(0);
    info.extend_from_slice(&LATE_ID_FUNCTION.value().to_le_bytes());
    info.extend_from_slice(&4_u32.to_le_bytes());
    info.push(0);
    let length = u32::try_from(info.len() - 4).unwrap();
    info[..4].copy_from_slice(&length.to_le_bytes());
    MockObjectFile::new(LATE_ID_IMAGE.value(), 0x2000)
        .with_dwarf(SectionId::DebugInfo, &info)
        .with_dwarf(SectionId::DebugAbbrev, &abbrev)
        .with_dwarf(SectionId::DebugLine, &[0])
        .with_hint(
            LATE_ID_FUNCTION.value(),
            UnwindHint::Leaf {
                stack_size: 0,
                saved: SavedRegisterSlots::default(),
            },
        )
        .with_build_id(ferros_core::BuildId::new(build_id).expect("build id"))
        .to_bytes()
}

/// A process stopped on the first instruction of `LATE_ID_FUNCTION`, with its caller in the
/// link register and no frame pointer: only the image file's leaf hint finds the caller.
/// The image at `path` is reported with `build_id`.
fn late_id_process(path: &std::path::Path, build_id: ferros_core::BuildIdentity) -> MockBackend
{
    let mut mapped = MockObjectImage::new(0..1, 0, 0)
        .with_mapping(LATE_ID_IMAGE.value(), 0x2000)
        .header_bytes();
    mapped.resize(0x2000, 0x1F);
    let mut registers = arm64_registers();
    registers.pc = LATE_ID_FUNCTION;
    registers.sp = Address::new(0x7_FFF0_0000);
    registers.general[30] = LATE_ID_CALLER.value();
    MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(LATE_ID_IMAGE, "r-x", mapped)
        .with_region(LATE_ID_CALLER, "r-x", vec![0x1F; 0x100])
        .with_image(late_id_listed(path, build_id))
}

fn late_id_listed(path: &std::path::Path, build_id: ferros_core::BuildIdentity) -> ferros_core::backend::LoadedImage
{
    ferros_core::backend::LoadedImage {
        path: path.to_path_buf(),
        load_address: LATE_ID_IMAGE,
        build_id,
        role: ferros_core::backend::ImageRole::MainExecutable,
    }
}

/// The function DWARF names at `LATE_ID_FUNCTION`, or why the session has none.
fn late_id_name(session: &mut Session) -> Result<Option<String>, ferros_core::DebuggerError>
{
    Ok(session.symbolicate(LATE_ID_FUNCTION)?.and_then(|symbolication| {
        symbolication
            .frames
            .last()
            .map(|frame| frame.symbol.display_name().to_owned())
    }))
}

/// The pcs of the stopped thread's frames.
fn late_id_pcs(session: &mut Session) -> Vec<Address>
{
    session
        .stack_trace(8)
        .expect("stack trace")
        .iter()
        .map(|frame| frame.pc)
        .collect()
}

/// The name the stopped thread's innermost frame is given.
fn late_id_top_name(session: &mut Session) -> Option<String>
{
    session
        .stack_trace(8)
        .expect("stack trace")
        .first()
        .and_then(|frame| frame.symbol.as_ref().map(|symbol| symbol.display_name().to_owned()))
}

/// #221: an image file opened while the mapped build id was unknown is served only while
/// no known id contradicts it. Once the loader reports another build than the file
/// records, the session stops unwinding with the file's unwind hints (the executable half)
/// and naming code from its DWARF (the debug half), and reads the file again: the old file
/// is refused as another build, and a file of the mapped build is loaded once it appears.
#[test]
fn contradictory_build_id_stops_serving_the_old_file()
{
    let (mut file, path) = ferros_tests::scratch::create_file("ferros-mock-late-build-id", "");
    std::io::Write::write_all(&mut file, &late_id_file("old_build", &[0xA1; 16])).expect("write image");
    drop(file);
    let _files = RemoveOnDrop(vec![path.clone()]);
    let mock = late_id_process(&path, ferros_core::BuildIdentity::Unknown);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    assert_eq!(late_id_name(&mut session).expect("symbolicate").as_deref(), Some("old_build"));
    assert_eq!(
        late_id_pcs(&mut session),
        vec![LATE_ID_FUNCTION, LATE_ID_CALLER],
        "the file's leaf hint finds the caller"
    );

    // The loader now reads the mapped id, and it is not the build the file records.
    mock.set_images(vec![late_id_listed(&path, known(&[0xB2; 16]))]);
    match late_id_name(&mut session) {
        Err(ferros_core::DebuggerError::SymbolLoad { .. }) => {}
        other => panic!("the old file's DWARF is still served: {other:?}"),
    }
    assert_ne!(
        late_id_top_name(&mut session).as_deref(),
        Some("old_build"),
        "the stack trace still names the frame from the old file's DWARF"
    );
    assert_eq!(
        late_id_pcs(&mut session),
        vec![LATE_ID_FUNCTION],
        "the old file's unwind hint is still served"
    );

    // A file of the mapped build replaces it; the next request loads both halves from it.
    std::fs::write(&path, late_id_file("new_build_fn", &[0xB2; 16])).expect("rewrite image");
    assert_eq!(
        late_id_name(&mut session).expect("symbolicate").as_deref(),
        Some("new_build_fn")
    );
    assert_eq!(late_id_pcs(&mut session), vec![LATE_ID_FUNCTION, LATE_ID_CALLER]);
}

/// [`MockObjectFormat`], counting how often it opens an image file.
struct CountingOpens(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ferros_core::objfile::ObjectFormat for CountingOpens
{
    fn mapped_symbols(
        &self,
        memory: &mut dyn ferros_core::objfile::TargetMemory,
        load_address: Address,
    ) -> Result<std::sync::Arc<dyn ferros_core::objfile::MappedSymbols>, String>
    {
        ferros_core::objfile::ObjectFormat::mapped_symbols(&MockObjectFormat, memory, load_address)
    }

    fn map_image(
        &self,
        memory: &mut dyn ferros_core::objfile::TargetMemory,
        load_address: Address,
    ) -> ferros_core::objfile::MappedImage
    {
        ferros_core::objfile::ObjectFormat::map_image(&MockObjectFormat, memory, load_address)
    }

    fn open_image(
        &self,
        path: &std::path::Path,
        load_address: u64,
        slice: Option<&ferros_core::objfile::ImageSlice>,
    ) -> ferros_core::Result<ferros_core::objfile::ImageContents>
    {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ferros_core::objfile::ObjectFormat::open_image(&MockObjectFormat, path, load_address, slice)
    }
}

/// #221: a late build id that matches the file opened while it was unknown keeps that
/// file: both halves are served without opening it again.
#[test]
fn a_matching_late_build_id_reuses_the_open_file()
{
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (mut file, path) = ferros_tests::scratch::create_file("ferros-mock-late-build-id-match", "");
    std::io::Write::write_all(&mut file, &late_id_file("same_build", &[0xC4; 16])).expect("write image");
    drop(file);
    let _files = RemoveOnDrop(vec![path.clone()]);
    let mock = late_id_process(&path, ferros_core::BuildIdentity::Unknown);
    let opens = std::sync::Arc::new(AtomicUsize::new(0));
    let mut session = Session::new(Box::new(mock.clone()), std::sync::Arc::new(CountingOpens(opens.clone())));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    assert_eq!(
        late_id_name(&mut session).expect("symbolicate").as_deref(),
        Some("same_build")
    );
    assert_eq!(late_id_pcs(&mut session), vec![LATE_ID_FUNCTION, LATE_ID_CALLER]);
    assert_eq!(opens.load(Ordering::SeqCst), 1);

    mock.set_images(vec![late_id_listed(&path, known(&[0xC4; 16]))]);
    assert_eq!(
        late_id_name(&mut session).expect("symbolicate").as_deref(),
        Some("same_build")
    );
    assert_eq!(late_id_pcs(&mut session), vec![LATE_ID_FUNCTION, LATE_ID_CALLER]);
    assert_eq!(opens.load(Ordering::SeqCst), 1, "a matching late id reopened the file");
}

/// [`MockObjectFormat`] whose mapped header never yields a slice: the format cannot tell
/// which build is mapped, so only the session can carry the build id the loader reports
/// to the file it opens.
struct NoSlice;

impl ferros_core::objfile::ObjectFormat for NoSlice
{
    fn mapped_symbols(
        &self,
        memory: &mut dyn ferros_core::objfile::TargetMemory,
        load_address: Address,
    ) -> Result<std::sync::Arc<dyn ferros_core::objfile::MappedSymbols>, String>
    {
        ferros_core::objfile::ObjectFormat::mapped_symbols(&MockObjectFormat, memory, load_address)
    }

    fn map_image(
        &self,
        memory: &mut dyn ferros_core::objfile::TargetMemory,
        load_address: Address,
    ) -> ferros_core::objfile::MappedImage
    {
        let mapped = ferros_core::objfile::ObjectFormat::map_image(&MockObjectFormat, memory, load_address);
        ferros_core::objfile::MappedImage {
            mapping: mapped.mapping,
            slice: None,
        }
    }

    fn open_image(
        &self,
        path: &std::path::Path,
        load_address: u64,
        slice: Option<&ferros_core::objfile::ImageSlice>,
    ) -> ferros_core::Result<ferros_core::objfile::ImageContents>
    {
        ferros_core::objfile::ObjectFormat::open_image(&MockObjectFormat, path, load_address, slice)
    }
}

/// #221: when the mapped header gives no slice, the format cannot refuse a file of another
/// build by itself. Once a late build id contradicts the opened file, opening the same
/// unchanged file again must not serve it for the mapped build.
#[test]
fn a_contradicted_file_is_refused_when_reopened_without_a_slice()
{
    let (mut file, path) = ferros_tests::scratch::create_file("ferros-mock-late-build-id-no-slice", "");
    std::io::Write::write_all(&mut file, &late_id_file("old_build", &[0xD5; 16])).expect("write image");
    drop(file);
    let _files = RemoveOnDrop(vec![path.clone()]);
    let mock = late_id_process(&path, ferros_core::BuildIdentity::Unknown);
    let mut session = Session::new(Box::new(mock.clone()), std::sync::Arc::new(NoSlice));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    assert_eq!(late_id_name(&mut session).expect("symbolicate").as_deref(), Some("old_build"));
    assert_eq!(late_id_pcs(&mut session), vec![LATE_ID_FUNCTION, LATE_ID_CALLER]);

    mock.set_images(vec![late_id_listed(&path, known(&[0xE6; 16]))]);
    match late_id_name(&mut session) {
        Err(ferros_core::DebuggerError::SymbolLoad { .. }) => {}
        other => panic!("the reopened old file is served for the mapped build: {other:?}"),
    }
    assert_eq!(
        late_id_pcs(&mut session),
        vec![LATE_ID_FUNCTION],
        "the reopened old file's unwind hint is served for the mapped build"
    );

    std::fs::write(&path, late_id_file("new_build_fn", &[0xE6; 16])).expect("rewrite image");
    assert_eq!(
        late_id_name(&mut session).expect("symbolicate").as_deref(),
        Some("new_build_fn")
    );
}

/// `path` spelled relative to the current directory, resolving to the same file.
fn late_id_relative(path: &std::path::Path) -> std::path::PathBuf
{
    let here = std::env::current_dir()
        .expect("current directory")
        .canonicalize()
        .expect("canonical current directory");
    let mut relative = std::path::PathBuf::new();
    for _ in here.components().skip(1) {
        relative.push("..");
    }
    relative.push(path.strip_prefix("/").expect("absolute path"));
    assert!(relative.is_relative());
    relative
}

/// #249: the loader lists one mapping twice, under a relative and an absolute spelling of
/// one file, so both listings share the symbol cache's entry for that file. When only one
/// listing reports a late build id that contradicts the file, neither listing may keep the
/// old file served: not its DWARF, not its unwind hint. Checked with the contradicting id
/// on either listing.
#[test]
fn a_contradicting_late_id_on_one_alias_stops_serving_the_old_file_for_both()
{
    use ferros_core::backend::ImageRole;

    for contradicted in [0_usize, 1] {
        let (mut file, path) = ferros_tests::scratch::create_file("ferros-mock-late-build-id-alias", "");
        std::io::Write::write_all(&mut file, &late_id_file("old_build", &[0xF1; 16])).expect("write image");
        drop(file);
        let _files = RemoveOnDrop(vec![path.clone()]);
        let absolute = path.canonicalize().expect("canonical image path");
        let relative = late_id_relative(&absolute);
        assert_eq!(relative.canonicalize().expect("relative spelling resolves"), absolute);
        let listings = |relative_id, absolute_id| {
            let mut relative = late_id_listed(&relative, relative_id);
            relative.role = ImageRole::Library;
            vec![relative, late_id_listed(&absolute, absolute_id)]
        };
        let mock = late_id_process(&absolute, ferros_core::BuildIdentity::Unknown);
        let unknown = ferros_core::BuildIdentity::Unknown;
        mock.set_images(listings(unknown, unknown));
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        assert_eq!(late_id_name(&mut session).expect("symbolicate").as_deref(), Some("old_build"));
        assert_eq!(late_id_pcs(&mut session), vec![LATE_ID_FUNCTION, LATE_ID_CALLER]);

        // Only one listing learns the mapped id, and it is not the file's build.
        let late = known(&[0xF2; 16]);
        mock.set_images(if contradicted == 0 {
            listings(late, unknown)
        } else {
            listings(unknown, late)
        });
        match late_id_name(&mut session) {
            Err(ferros_core::DebuggerError::SymbolLoad { .. }) => {}
            other => panic!("listing {contradicted} contradicted: the old file's DWARF is still served: {other:?}"),
        }
        assert_eq!(
            late_id_pcs(&mut session),
            vec![LATE_ID_FUNCTION],
            "listing {contradicted} contradicted: the old file's unwind hint is still served"
        );

        std::fs::write(&path, late_id_file("new_build_fn", &[0xF2; 16])).expect("rewrite image");
        assert_eq!(
            late_id_name(&mut session).expect("symbolicate").as_deref(),
            Some("new_build_fn")
        );
        assert_eq!(late_id_pcs(&mut session), vec![LATE_ID_FUNCTION, LATE_ID_CALLER]);
    }
}
