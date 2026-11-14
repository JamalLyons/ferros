//! Debug-info discovery and loader relocation regressions for issue #34.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferros_core::backend::LoadedImage;
use ferros_core::backend::mock::MockBackend;
use ferros_core::symbols::{BinaryImage, DebugInfoOptions, ImageDescriptor, ImageMapping};
use ferros_core::{
    Address, BreakpointRequest, DebuggerError, LaunchSpec, Registers, Session, StopEvent, SymbolCache, ThreadId,
};
use ferros_macos::MacOSBackend;
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture, harness};
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

struct Scratch(PathBuf);

impl Scratch
{
    fn new() -> Self
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "ferros-debuginfo-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create scratch directory: {error}"),
            }
        }
    }

    fn options(&self) -> DebugInfoOptions
    {
        DebugInfoOptions {
            cache_directory: self.0.join("cache"),
            dsymutil: None,
            tool_timeout: Duration::from_secs(10),
        }
    }
}

impl Drop for Scratch
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn descriptor(path: &Path, load_address: u64) -> ImageDescriptor
{
    ImageDescriptor {
        path: path.to_path_buf(),
        load_address,
    }
}

/// Map `path`'s Mach-O header and load commands at `load_address`, as the loader would:
/// the session reads an image's segment layout from target memory, not from its file.
fn with_mapped_header(mock: MockBackend, path: &Path, load_address: u64) -> MockBackend
{
    let bytes = fs::read(path).expect("read executable");
    let size = ImageMapping::header_size(&bytes).expect("Mach-O header");
    mock.with_region(Address::new(load_address), "r-x", &bytes[..size])
}

/// Read the symbol table independently of the engine's DWARF resolver.
fn symbol(path: &Path, name: &str) -> (u64, u64)
{
    let bytes = fs::read(path).expect("read executable");
    let file = object::File::parse(&*bytes).expect("parse executable");
    let symbol = file
        .symbols()
        .find(|entry| {
            entry.kind() == SymbolKind::Text
                && entry.is_definition()
                && entry
                    .name()
                    .ok()
                    .and_then(|raw| rustc_demangle::try_demangle(raw).ok())
                    .is_some_and(|demangled| format!("{demangled:#}") == name)
        })
        .unwrap_or_else(|| panic!("no symbol {name}"));
    (symbol.address(), symbol.size().max(256))
}

fn assert_source_line(image: &BinaryImage, executable: &Path, fixture: &Fixture)
{
    let (start, size) = symbol(executable, "ferros_fixture_hello::main");
    let expected = fixture.marker_line("hello_print");
    assert!(
        (start..start + size).step_by(4).any(|file| {
            image
                .runtime_address(file)
                .and_then(|runtime| image.symbolicate(runtime))
                .is_some_and(|result| {
                    result.frames.iter().any(|frame| {
                        rustc_demangle::try_demangle(frame.symbol.raw())
                            .is_ok_and(|name| format!("{name:#}") == "ferros_fixture_hello::main")
                            && frame.location.as_ref().is_some_and(|location| {
                                location.file.ends_with("hello.rs") && location.line == Some(expected)
                            })
                    })
                })
        }),
        "main must resolve to hello.rs at source marker hello_print"
    );
}

fn expect_no_debug_info(result: ferros_core::Result<std::sync::Arc<BinaryImage>>)
{
    let Err(error) = result else {
        panic!("unexpected debug info");
    };
    assert!(matches!(error, DebuggerError::NoDebugInfo { .. }), "{error}");
    let message = error.to_string();
    assert!(message.contains("split-debuginfo"), "{message}");
    assert!(message.contains("dsymutil"), "{message}");
}

#[test]
fn packed_fixture_discovers_uuid_matching_dsym()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let mut cache = SymbolCache::with_debug_info_options(scratch.options());
        let image = cache
            .load_image(descriptor(fixture.path(), 0x2_0000_0000))
            .expect("adjacent dSYM");
        assert_eq!(image.path(), fixture.path());
        assert_source_line(&image, fixture.path(), &fixture);
        assert!(!scratch.0.join("cache").exists(), "adjacent lookup must not generate DWARF");
    });
}

#[test]
fn mismatched_dsym_is_rejected_and_no_tool_has_actionable_typed_error()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        fs::copy(fixture.path(), &executable).expect("copy executable without companion");
        let mut cache = SymbolCache::with_debug_info_options(scratch.options());
        expect_no_debug_info(cache.load_image(descriptor(&executable, 0x2_0000_0000)));

        let directory = scratch.0.join("program.dSYM/Contents/Resources/DWARF");
        fs::create_dir_all(&directory).expect("create adjacent bundle");
        let wrong_fixture = fixture!("nested_calls");
        let wrong_member = fs::read_dir(wrong_fixture.dsym_path().join("Contents/Resources/DWARF"))
            .expect("wrong bundle")
            .next()
            .expect("DWARF member")
            .expect("directory entry")
            .path();
        fs::copy(wrong_member, directory.join("arbitrary-name")).expect("copy mismatched DWARF");
        expect_no_debug_info(cache.load_image(descriptor(&executable, 0x2_0000_0000)));
    });
}

#[test]
fn default_cargo_build_uses_dsymutil_then_reuses_validated_cache()
{
    harness::run_with_timeout(Duration::from_secs(90), |_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        // This standalone package has no profile override and inherits no workspace
        // profile. Keep its object files alive for the entire dsymutil lookup.
        fs::write(
            scratch.0.join("Cargo.toml"),
            format!(
                "[package]\nname = \"ferros-fixture-hello\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \
                 \"ferros-fixture-hello\"\npath = {:?}\n",
                fixture.source_path().to_str().expect("source path")
            ),
        )
        .expect("write default package");
        let output = Command::new(env!("CARGO"))
            .current_dir(&scratch.0)
            .args(["build", "--offline", "--message-format=json"])
            .env("CARGO_TARGET_DIR", scratch.0.join("cargo-output"))
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO")
            .env_remove("CARGO_PROFILE_DEV_DEBUG")
            .output()
            .expect("plain cargo build");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let executable = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|message| message["executable"].as_str().map(PathBuf::from))
            .expect("Cargo executable artifact");
        let bytes = fs::read(&executable).expect("read unpacked executable");
        let file = object::File::parse(&*bytes).expect("parse unpacked executable");
        assert!(file.section_by_name("__debug_info").is_none_or(|section| section.size() == 0));
        assert!(!PathBuf::from(format!("{}.dSYM", executable.display())).exists());

        let mut options = scratch.options();
        options.dsymutil = Some(PathBuf::from("/usr/bin/dsymutil"));
        let mut cache = SymbolCache::with_debug_info_options(options);
        let image = cache
            .load_image(descriptor(&executable, 0x3_0000_0000))
            .expect("dsymutil fallback");
        assert_source_line(&image, &executable, &fixture);
        let entries: Vec<_> = fs::read_dir(scratch.0.join("cache")).expect("cache").collect();
        assert_eq!(entries.len(), 1, "only validated output is published");

        // A new resolver with no tool can use the persisted artifact at another slide.
        let mut cache = SymbolCache::with_debug_info_options(scratch.options());
        let image = cache
            .load_image(descriptor(&executable, 0x4_0000_0000))
            .expect("UUID cache hit");
        assert_source_line(&image, &executable, &fixture);
        // Poison the cache with another UUID: a cache filename is not proof of identity.
        let entry = fs::read_dir(scratch.0.join("cache"))
            .expect("cache")
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::copy(fixture!("nested_calls").path(), entry).expect("replace cache with wrong UUID");
        let mut cache = SymbolCache::with_debug_info_options(scratch.options());
        expect_no_debug_info(cache.load_image(descriptor(&executable, 0x3_0000_0000)));
    });
}

#[test]
fn failed_and_empty_tool_output_is_not_published()
{
    harness::run(|_| {
        for tool in ["/usr/bin/false", "/usr/bin/true", "/nonexistent/ferros-dsymutil"] {
            let scratch = Scratch::new();
            let executable = scratch.0.join("program");
            fs::copy(fixture!("hello").path(), &executable).expect("copy executable");
            let mut options = scratch.options();
            options.dsymutil = Some(tool.into());
            let mut cache = SymbolCache::with_debug_info_options(options);
            expect_no_debug_info(cache.load_image(descriptor(&executable, 0x2_0000_0000)));
            assert_eq!(fs::read_dir(scratch.0.join("cache")).expect("cache directory").count(), 0);
        }
    });
}

#[test]
fn timed_out_tool_is_killed_and_temporary_output_is_removed()
{
    harness::run(|_| {
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        fs::copy(fixture!("hello").path(), &executable).expect("copy executable");
        let tool = scratch.0.join("slow-dsymutil");
        fs::write(&tool, "#!/bin/sh\nexec /bin/sleep 5\n").expect("tool script");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).expect("executable script");
        let mut options = scratch.options();
        options.dsymutil = Some(tool);
        options.tool_timeout = Duration::from_millis(20);
        let start = std::time::Instant::now();
        let mut cache = SymbolCache::with_debug_info_options(options);
        let Err(error) = cache.load_image(descriptor(&executable, 0x2_0000_0000)) else {
            panic!("unexpected symbols");
        };
        assert!(matches!(error, DebuggerError::NoDebugInfo { .. }));
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(3), "tool deadline was not enforced");
        assert_eq!(fs::read_dir(scratch.0.join("cache")).expect("cache").count(), 0);
    });
}

#[test]
fn session_preserves_typed_missing_debug_info_but_returns_raw_stack_frames()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        fs::copy(fixture.path(), &executable).expect("copy executable without dSYM");
        let (file, _) = symbol(&executable, "ferros_fixture_hello::main");
        let address = ImageMapping::parse(&descriptor(&executable, 0x2_0000_0000))
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mut registers = Registers::default();
        registers.pc = address;
        let mock = with_mapped_header(MockBackend::new(), &executable, 0x2_0000_0000)
            .with_thread(ThreadId(1), registers)
            .with_image(LoadedImage {
                path: executable,
                load_address: Address::new(0x2_0000_0000),
            });
        let mut session = Session::new(Box::new(mock)).with_debug_info_options(scratch.options());
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");
        for _ in 0..2 {
            let error = session.symbolicate(address).expect_err("missing debug info");
            assert!(matches!(error, DebuggerError::NoDebugInfo { .. }));
            assert!(
                error.to_string().contains("disabled"),
                "configured tool selection must survive launch"
            );
            assert!(error.to_string().contains("split-debuginfo"));
        }
        let frames = session.stack_trace(1).expect("raw stack");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pc, address);
        assert!(frames[0].symbol.is_none());
    });
}

#[test]
fn session_refreshes_unloaded_and_relocated_images()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let first = LoadedImage {
            path: fixture.path().to_path_buf(),
            load_address: Address::new(0x2_0000_0000),
        };
        let second = LoadedImage {
            load_address: Address::new(0x3_0000_0000),
            ..first.clone()
        };
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let old_address = ImageMapping::parse(&descriptor(fixture.path(), first.load_address.value()))
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mock = MockBackend::new();
        let mock = with_mapped_header(mock, fixture.path(), first.load_address.value());
        let mock = with_mapped_header(mock, fixture.path(), second.load_address.value())
            .with_thread(ThreadId(1), Registers::default())
            .with_image(first.clone());
        let mut session = Session::new(Box::new(mock.clone()));
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");
        assert!(session.symbolicate(old_address).expect("symbolicate").is_some());
        assert_eq!(
            session.file_address(old_address).expect("file address"),
            Some((first.path.clone(), file))
        );
        mock.set_images(vec![second.clone()]);
        assert!(session.symbolicate(old_address).expect("unloaded image").is_none());
        assert!(session.runtime_address(&first, file).expect("old mapping").is_none());
        let new_address = session.runtime_address(&second, file).expect("new mapping").expect("address");
        assert_ne!(old_address, new_address);
        assert!(session.symbolicate(new_address).expect("relocated symbol").is_some());
    });
}

#[test]
fn relative_image_paths_keep_symbols_across_refreshes()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let current = std::env::current_dir().expect("cwd");
        let relative = current
            .ancestors()
            .enumerate()
            .find_map(|(levels, ancestor)| {
                let suffix = fixture.path().strip_prefix(ancestor).ok()?;
                let mut path = PathBuf::new();
                for _ in 0..levels {
                    path.push("..");
                }
                path.push(suffix);
                Some(path)
            })
            .expect("relative fixture path");
        assert!(!relative.is_absolute());
        let image = LoadedImage {
            path: relative.clone(),
            load_address: Address::new(0x2_0000_0000),
        };
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let address = ImageMapping::parse(&descriptor(&relative, image.load_address.value()))
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mock = with_mapped_header(MockBackend::new(), fixture.path(), image.load_address.value())
            .with_thread(ThreadId(1), Registers::default())
            .with_image(image);
        let mut session = Session::new(Box::new(mock));
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");
        assert!(session.symbolicate(address).expect("first lookup").is_some());
        assert!(session.symbolicate(address).expect("second lookup").is_some());
    });
}

#[test]
fn two_aslr_launches_symbolicate_and_report_dyld_libraries()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut loads = Vec::new();
        for _ in 0..2 {
            let mut debuggee = t.launch(&fixture);
            let address = function_address(&mut debuggee, &fixture, "level_c");
            debuggee
                .add_breakpoint(BreakpointRequest::Software { address })
                .expect("breakpoint before dyld initializes");
            debuggee.resume().expect("resume");
            assert!(matches!(
                debuggee.wait(Some(Duration::from_secs(5))).expect("wait"),
                Some(StopEvent::Breakpoint { pc, .. }) if pc == address
            ));
            let images = debuggee.images().expect("dyld images");
            assert!(images.len() > 1, "dyld must enumerate libraries as well as the executable");
            for library in images.iter().filter(|image| image.path != fixture.path()) {
                assert_eq!(
                    debuggee.read_memory(library.load_address, 4).expect("loaded library header"),
                    0xfeed_facf_u32.to_le_bytes(),
                    "{}",
                    library.path.display()
                );
            }
            let main = images.iter().find(|image| image.path == fixture.path()).expect("main image");
            loads.push(main.load_address);
            let result = debuggee.symbolicate(address).expect("symbolication").expect("known function");
            assert!(result.frames.iter().any(|frame| {
                rustc_demangle::try_demangle(frame.symbol.raw())
                    .is_ok_and(|name| format!("{name:#}") == "ferros_fixture_nested_calls::level_c")
            }));
            let (path, file) = debuggee.file_address(address).expect("file address").expect("mapped address");
            assert_eq!(path, fixture.path());
            assert_eq!(debuggee.runtime_address(main, file).expect("runtime address"), Some(address));
        }
        assert_ne!(loads[0], loads[1], "the test must exercise different ASLR slides");
    });
}

/// Issue #78: every Cargo command, even a fresh one, replaces the uplifted
/// `target/debug/ferros-fixture-*` file by removing it and creating it again, so a
/// concurrent `cargo test` can leave the path of a running fixture briefly missing.
/// Mapping a loaded image must use the Mach-O header the loader mapped, not whatever file
/// the path names at lookup time. Removing a private copy after it starts, then
/// attaching as the reported single-step test does, reproduces that window.
#[test]
fn loaded_image_maps_after_its_file_is_removed()
{
    harness::run(|t| {
        let scratch = Scratch::new();
        let directory = fs::canonicalize(&scratch.0).expect("canonical scratch directory");
        let path = directory.join("ferros-fixture-spawn_thread");
        fs::copy(fixture!("spawn_thread").path(), &path).expect("copy the fixture");
        let bytes = fs::read(&path).expect("read the copy");
        let (file, _) = symbol(&path, "ferros_fixture_spawn_thread::spawn_later");
        let expected = {
            let object = object::File::parse(&*bytes).expect("parse the copy");
            let section = object
                .sections()
                .find(|section| (section.address()..section.address() + section.size()).contains(&file))
                .expect("section holding spawn_later");
            let (offset, _) = section.file_range().expect("spawn_later is in the file");
            let start = usize::try_from(offset + file - section.address()).expect("file offset");
            bytes[start..start + 16].to_vec()
        };
        let path_text: &'static str = Box::leak(path.to_str().expect("UTF-8 scratch path").into());
        let target = t.spawn(&Fixture::from_built("spawn_thread", path_text), &["pause=4000"]);
        let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
        // Attach once the first worker runs: dyld has then published the image list, as
        // it had in the reported failure, so the main image's path is the loader's.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match session.attach(target.pid()) {
                Ok(()) if session.threads().expect("threads").len() >= 2 => break,
                Ok(()) => session.detach().expect("detach before the worker runs"),
                Err(error) if Instant::now() >= deadline => panic!("attach failed: {error}"),
                Err(_) => {}
            }
            assert!(Instant::now() < deadline, "the fixture's first worker did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::remove_file(&path).expect("remove the running executable");
        let images = session.images().expect("images");
        let main = images.iter().find(|image| image.path == path).expect("main image");
        let address = session
            .runtime_address(main, file)
            .expect("image mapping")
            .expect("spawn_later is mapped while its file is missing");
        assert_eq!(session.read_memory(address, 16).expect("read spawn_later"), expected);
        assert_eq!(
            session.file_address(address).expect("file address"),
            Some((path.clone(), file))
        );
    });
}

/// Issue #83: at the launch-entry stop dyld has published no images, so the backend
/// reports the main executable itself. Its path must not depend on the file still
/// existing: a Cargo command can unlink a running fixture's executable at any time.
#[test]
fn bootstrap_main_image_is_reported_after_its_file_is_removed()
{
    harness::run(|t| {
        let scratch = Scratch::new();
        let directory = fs::canonicalize(&scratch.0).expect("canonical scratch directory");
        let path = directory.join("ferros-fixture-hello");
        fs::copy(fixture!("hello").path(), &path).expect("copy the fixture");
        let header = fs::read(&path).expect("read the copy")[..32].to_vec();
        let path_text: &'static str = Box::leak(path.to_str().expect("UTF-8 scratch path").into());
        let mut debuggee = t.launch(&Fixture::from_built("hello", path_text));
        fs::remove_file(&path).expect("remove the running executable");
        let images = debuggee.images().expect("images at the bootstrap stop");
        let main = images.first().expect("the main image");
        assert_eq!(main.path, path);
        assert_eq!(
            debuggee.read_memory(main.load_address, 32).expect("read the mapped header"),
            header
        );
    });
}
