//! Images without source debug information (issue #236): the symbol cache does not reread
//! one for every debug-info request, and a source step and a line breakpoint through one
//! report the missing debug information instead of a wrong location.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ferros_core::backend::StdioSpec;
use ferros_core::objfile::mock::{MockObjectFile, MockObjectFormat};
use ferros_core::objfile::{ImageContents, ImageSlice, MappedImage, MappedSymbols, ObjectFormat, TargetMemory};
use ferros_core::symbols::ImageDescriptor;
use ferros_core::{
    Address, BreakpointLocation, BreakpointRequest, DebuggerError, LaunchSpec, Session, StopEvent, SymbolCache, ThreadId,
};
use ferros_macos::{DebugInfoOptions, MacOSBackend, MacOSRuntime, MachOFormat};
use ferros_tests::stripped::StrippedFixture;
use ferros_tests::{Fixture, fixture, harness, scratch};

/// [`MockObjectFormat`], counting how often it opens an image file.
struct CountingFormat(Arc<AtomicUsize>);

impl ObjectFormat for CountingFormat
{
    fn mapped_symbols(&self, memory: &mut dyn TargetMemory, load_address: Address)
    -> Result<Arc<dyn MappedSymbols>, String>
    {
        MockObjectFormat.mapped_symbols(memory, load_address)
    }

    fn map_image(&self, memory: &mut dyn TargetMemory, load_address: Address) -> MappedImage
    {
        MockObjectFormat.map_image(memory, load_address)
    }

    fn open_image(&self, path: &Path, load_address: u64, slice: Option<&ImageSlice>) -> ferros_core::Result<ImageContents>
    {
        self.0.fetch_add(1, Ordering::SeqCst);
        MockObjectFormat.open_image(path, load_address, slice)
    }
}

/// A repeated debug-info request for an image file that has none returns the kept
/// missing-debug-info error without opening the file again, until the file changes.
#[test]
fn cache_does_not_reread_an_unchanged_image_without_debug_info()
{
    harness::run(|_| {
        let directory = scratch::create_dir("ferros-no-debug-info");
        let path = directory.join("image.mock");
        fs::write(&path, MockObjectFile::new(0x1000, 0x1000).to_bytes()).expect("write image without DWARF");
        let opens = Arc::new(AtomicUsize::new(0));
        let mut cache = SymbolCache::new(Arc::new(CountingFormat(opens.clone())));
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x2_0000_0000,
        };

        for _ in 0..3 {
            let Err(error) = cache.load_image(descriptor.clone()) else {
                panic!("unexpected debug info");
            };
            assert!(matches!(error, DebuggerError::NoDebugInfo { .. }), "{error}");
        }
        assert_eq!(opens.load(Ordering::SeqCst), 1, "an unchanged image was read again");
        assert!(cache.image_for_address(Address::new(0x2_0000_0000)).is_none());

        // Another load address is another image: it is read once too.
        let moved = ImageDescriptor {
            load_address: 0x3_0000_0000,
            ..descriptor.clone()
        };
        for _ in 0..2 {
            assert!(matches!(
                cache.load_image(moved.clone()),
                Err(DebuggerError::NoDebugInfo { .. })
            ));
        }
        assert_eq!(opens.load(Ordering::SeqCst), 2);

        // A changed file (another size) is read again.
        let before = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("image mtime");
        fs::write(&path, MockObjectFile::new(0x1000, 0x2000).to_bytes()).expect("rewrite image");
        // Both files have the same length: move the mtime so the change is seen even on a
        // filesystem with coarse timestamps.
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(before + Duration::from_secs(2)))
            .expect("move the rewritten image's mtime");
        assert!(matches!(cache.load_image(descriptor), Err(DebuggerError::NoDebugInfo { .. })));
        assert_eq!(opens.load(Ordering::SeqCst), 3, "a changed image was not read again");
        let _ = fs::remove_dir_all(directory);
    });
}

/// A rewrite in place that keeps the image's size and restores its modification time is
/// still seen as a change (#346): the status-change time moves, and the file is read again.
#[test]
fn cache_rereads_an_image_rewritten_with_its_size_and_mtime_kept()
{
    harness::run(|_| {
        let directory = scratch::create_dir("ferros-no-debug-info-rewrite");
        let path = directory.join("image.mock");
        let original = MockObjectFile::new(0x1000, 0x1000).to_bytes();
        fs::write(&path, &original).expect("write image without DWARF");
        let opens = Arc::new(AtomicUsize::new(0));
        let mut cache = SymbolCache::new(Arc::new(CountingFormat(opens.clone())));
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x2_0000_0000,
        };
        assert!(matches!(
            cache.load_image(descriptor.clone()),
            Err(DebuggerError::NoDebugInfo { .. })
        ));
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        let before = fs::metadata(&path).expect("image metadata");
        let modified = before.modified().expect("image mtime");
        let rewritten = MockObjectFile::new(0x1000, 0x2000).to_bytes();
        assert_eq!(rewritten.len(), original.len(), "the rewrite must keep the size");
        assert_ne!(rewritten, original);
        {
            let mut file = fs::File::options().write(true).open(&path).expect("open image in place");
            std::io::Write::write_all(&mut file, &rewritten).expect("rewrite image in place");
            file.set_modified(modified).expect("restore the image's mtime");
        }
        let after = fs::metadata(&path).expect("rewritten image metadata");
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().ok(), Some(modified), "the mtime was not restored");

        assert!(matches!(cache.load_image(descriptor), Err(DebuggerError::NoDebugInfo { .. })));
        assert_eq!(
            opens.load(Ordering::SeqCst),
            2,
            "a same-size rewrite with its mtime restored was not read again"
        );
        let _ = fs::remove_dir_all(directory);
    });
}

/// `frameless_stack` copied without its dSYM and with its debug map stripped, so its
/// main executable has no source debug information.
fn stripped_frameless_stack() -> StrippedFixture
{
    StrippedFixture::new(&fixture!("frameless_stack"), Path::new(env!("CARGO_TARGET_TMPDIR")))
}

/// Launch `fixture` with debug-info discovery confined to `cache` and no `dsymutil`.
fn launch(fixture: &Fixture, tag: &str, cache: &Path) -> Session
{
    let options = DebugInfoOptions {
        cache_directory: cache.to_path_buf(),
        dsymutil: None,
        tool_timeout: Duration::from_secs(10),
    };
    let mut session = Session::new(
        Box::new(MacOSBackend::new().expect("backend")),
        MachOFormat::with_debug_info_options(options).into_shared(),
    )
    .with_runtime(MacOSRuntime::shared());
    let spec = LaunchSpec::new(fixture.path())
        .arg(tag)
        .stdin(StdioSpec::Null)
        .stdout(StdioSpec::Null)
        .stderr(StdioSpec::Null);
    session.launch(&spec).expect("launch fixture");
    session
}

/// Stop on the `nop` of `_unwind_only_callee`, code of the stripped main executable.
fn stop_in_stripped_code(d: &mut Session, fixture: &Fixture) -> (Address, ThreadId)
{
    let leaf = ferros_tests::symbols::raw_symbol_address(d, fixture, "_unwind_only_callee");
    let stop = (0..64)
        .map(|index| Address::new(leaf.value() + 4 * index))
        .find(|&at| {
            let bytes = d.read_memory(at, 4).expect("read code");
            u32::from_le_bytes(bytes.try_into().expect("four bytes")) == 0xD503_201F // nop
        })
        .expect("the callee's nop");
    d.add_breakpoint(BreakpointRequest::Software { address: stop })
        .expect("break on the callee's nop");
    d.resume().expect("resume");
    let thread = match d.wait(Some(Duration::from_secs(10))).expect("wait") {
        Some(StopEvent::Breakpoint { pc, thread }) if pc == stop => thread,
        other => panic!("expected a breakpoint stop at {stop}, got {other:?}"),
    };
    d.select_thread(thread).expect("select the stopped thread");
    (stop, thread)
}

/// A line breakpoint in an executable without source debug information reports
/// [`DebuggerError::NoDebugInfo`] and adds nothing, repeatedly.
#[test]
fn line_breakpoint_in_an_image_without_debug_info_reports_no_debug_info()
{
    harness::run(|t| {
        let tag = t.tag();
        let stripped = stripped_frameless_stack();
        let cache = stripped.directory.join("cache");
        let mut d = launch(&stripped.fixture, tag.as_str(), &cache);
        let line = fixture!("frameless_stack").marker_line("frameless_done");
        for _ in 0..2 {
            let error = d
                .set_breakpoint(BreakpointLocation::Line {
                    file: "frameless_stack.rs".into(),
                    line,
                })
                .expect_err("no line table to resolve the breakpoint");
            assert!(
                matches!(&error, DebuggerError::NoDebugInfo { path, .. } if path == stripped.fixture.path()),
                "{error}"
            );
            assert!(d.logical_breakpoints().is_empty(), "a failed breakpoint was added");
        }
    });
}

/// A source step from code without source debug information runs one instruction (it
/// has no line to step over) and stops there; the new pc has no source position, and
/// symbolicating it reports [`DebuggerError::NoDebugInfo`].
#[test]
fn source_step_in_an_image_without_debug_info_steps_one_instruction()
{
    harness::run(|t| {
        let tag = t.tag();
        let stripped = stripped_frameless_stack();
        let cache = stripped.directory.join("cache");
        let mut d = launch(&stripped.fixture, tag.as_str(), &cache);
        let (stop, thread) = stop_in_stripped_code(&mut d, &stripped.fixture);
        let error = d.symbolicate(stop).expect_err("no source debug information");
        assert!(matches!(error, DebuggerError::NoDebugInfo { .. }), "{error}");

        d.step_over().expect("step over without line information");
        match d.wait(Some(Duration::from_secs(10))).expect("wait") {
            Some(StopEvent::SingleStep { thread: stepped }) if stepped == thread => {}
            other => panic!("expected the step to stop {thread:?}, got {other:?}"),
        }
        let pc = d.read_registers_for(thread).expect("registers").pc;
        assert_eq!(pc, Address::new(stop.value() + 4), "one instruction past the nop");
        let position = d.source_position(pc).expect("source position");
        assert_eq!(position.line, None, "a line without a line table: {position:?}");
        assert_eq!(position.file, None, "a file without a line table: {position:?}");
        let error = d.symbolicate(pc).expect_err("still no source debug information");
        assert!(
            matches!(&error, DebuggerError::NoDebugInfo { path, .. } if path == stripped.fixture.path()),
            "{error}"
        );
    });
}

/// A format that cannot open any image: it counts opens and reports
/// [`DebuggerError::NoDebugInfo`] from `open_image` itself, as a format does when it
/// cannot tell which part of a file the target mapped.
struct NoDebugAtOpen(Arc<AtomicUsize>);

impl ObjectFormat for NoDebugAtOpen
{
    fn mapped_symbols(&self, memory: &mut dyn TargetMemory, load_address: Address)
    -> Result<Arc<dyn MappedSymbols>, String>
    {
        MockObjectFormat.mapped_symbols(memory, load_address)
    }

    fn map_image(&self, memory: &mut dyn TargetMemory, load_address: Address) -> MappedImage
    {
        MockObjectFormat.map_image(memory, load_address)
    }

    fn open_image(&self, path: &Path, _: u64, _: Option<&ImageSlice>) -> ferros_core::Result<ImageContents>
    {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DebuggerError::NoDebugInfo {
            path: path.to_path_buf(),
            detail: "the mapped part of the file is unknown".into(),
        })
    }
}

/// `NoDebugInfo` reported by `ObjectFormat::open_image` itself is kept like missing
/// DWARF: a repeated request for the unchanged file does not open it again, and a
/// changed file is opened again (issue #244).
#[test]
fn cache_does_not_reopen_an_unchanged_image_whose_open_reports_no_debug_info()
{
    harness::run(|_| {
        let directory = scratch::create_dir("ferros-no-debug-at-open");
        let path = directory.join("image.mock");
        fs::write(&path, MockObjectFile::new(0x1000, 0x1000).to_bytes()).expect("write image");
        let opens = Arc::new(AtomicUsize::new(0));
        let mut cache = SymbolCache::new(Arc::new(NoDebugAtOpen(opens.clone())));
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x2_0000_0000,
        };

        for _ in 0..3 {
            let Err(error) = cache.load_image(descriptor.clone()) else {
                panic!("unexpected debug info");
            };
            assert!(
                matches!(&error, DebuggerError::NoDebugInfo { detail, .. } if detail.contains("mapped part")),
                "{error}"
            );
        }
        assert_eq!(opens.load(Ordering::SeqCst), 1, "an unchanged image was opened again");

        // A changed file (another size) is opened again.
        fs::write(&path, MockObjectFile::new(0x1000, 0x2000).to_bytes()).expect("rewrite image");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"more"))
            .expect("grow the image");
        assert!(matches!(cache.load_image(descriptor), Err(DebuggerError::NoDebugInfo { .. })));
        assert_eq!(opens.load(Ordering::SeqCst), 2, "a changed image was not opened again");
        let _ = fs::remove_dir_all(directory);
    });
}
