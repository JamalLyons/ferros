//! Debug-info discovery and loader relocation regressions for issue #34.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ferros_core::backend::mock::MockBackend;
use ferros_core::backend::{LoadedImage, StdioSpec};
use ferros_core::symbols::{BinaryImage, ImageDescriptor};
use ferros_core::{
    Address, BreakpointLocation, BreakpointRequest, DebuggerError, LaunchSpec, Registers, Session, StopEvent, SymbolCache,
    ThreadId,
};
use ferros_macos::{DebugInfoOptions, MacOSBackend, MacOSRuntime, MachOFormat};
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture, harness, scratch};
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

struct Scratch(PathBuf);

impl Scratch
{
    fn new() -> Self
    {
        Self(scratch::create_dir("ferros-debuginfo"))
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
    let size = ferros_macos::macho::header_size(&bytes).expect("Mach-O header");
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

/// `path` relative to the current directory, through `..` components.
fn relative_to_cwd(path: &Path) -> PathBuf
{
    let current = std::env::current_dir().expect("cwd");
    let relative = current
        .ancestors()
        .enumerate()
        .find_map(|(levels, ancestor)| {
            let suffix = path.strip_prefix(ancestor).ok()?;
            let mut relative = PathBuf::new();
            for _ in 0..levels {
                relative.push("..");
            }
            relative.push(suffix);
            Some(relative)
        })
        .expect("relative path");
    assert!(!relative.is_absolute());
    relative
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
        let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared());
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
        let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared());
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
        let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(options).into_shared());
        let image = cache
            .load_image(descriptor(&executable, 0x3_0000_0000))
            .expect("dsymutil fallback");
        assert_source_line(&image, &executable, &fixture);
        let entries: Vec<_> = fs::read_dir(scratch.0.join("cache")).expect("cache").collect();
        assert_eq!(entries.len(), 1, "only validated output is published");

        // A new resolver with no tool can use the persisted artifact at another slide.
        let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared());
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
        let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared());
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
            let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(options).into_shared());
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
        let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(options).into_shared());
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
        let address = ferros_macos::macho::file_mapping(&executable, 0x2_0000_0000)
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
                build_id: ferros_core::types::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
            });
        let mut session = Session::new(
            Box::new(mock),
            MachOFormat::with_debug_info_options(scratch.options()).into_shared(),
        );
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

/// Issue #85: a symbol load that fails for a reason other than missing debug info (here
/// the image's file is briefly missing, as while Cargo re-uplifts a fixture) is reported
/// with its original error, and a later request loads the symbols once the file is back.
#[test]
fn session_reports_a_transient_symbol_load_failure_and_retries_later()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        let link = || std::os::unix::fs::symlink(fixture.path(), &executable).expect("link the executable");
        std::os::unix::fs::symlink(fixture.dsym_path(), scratch.0.join("program.dSYM")).expect("link the dSYM");
        let load_address = 0x2_0000_0000;
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let address = ferros_macos::macho::file_mapping(fixture.path(), load_address)
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mock = with_mapped_header(MockBackend::new(), fixture.path(), load_address)
            .with_thread(ThreadId(1), Registers::default())
            .with_image(LoadedImage {
                path: executable.clone(),
                load_address: Address::new(load_address),
                build_id: ferros_core::types::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
            });
        let mut session = Session::new(
            Box::new(mock),
            MachOFormat::with_debug_info_options(scratch.options()).into_shared(),
        );
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");

        for _ in 0..2 {
            let error = session.symbolicate(address).expect_err("missing image file");
            let DebuggerError::SymbolLoad { path, source } = &error else {
                panic!("unexpected result {error}");
            };
            assert_eq!(path, &executable);
            assert!(
                matches!(&**source, DebuggerError::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
                "{source}"
            );
        }
        link();
        let symbolication = session
            .symbolicate(address)
            .expect("symbols once the file is back")
            .expect("main has a symbol");
        assert!(
            symbolication
                .frames
                .iter()
                .any(|frame| rustc_demangle::try_demangle(frame.symbol.raw())
                    .is_ok_and(|name| format!("{name:#}") == "ferros_fixture_hello::main")),
            "{symbolication:?}"
        );
    });
}

/// Issue #100: a symbol load that keeps failing (here a corrupt image file) is read and
/// parsed once, not on every request. While the file is unchanged, every request returns
/// the original error: after the file is made unreadable, a re-read would report
/// permission denied instead. Once the file changes, the next request reads it again.
#[test]
fn session_does_not_reread_an_unchanged_image_whose_symbol_load_failed()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        fs::write(&executable, b"not a Mach-O image").expect("write a corrupt image");
        let load_address = 0x2_0000_0000;
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let address = ferros_macos::macho::file_mapping(fixture.path(), load_address)
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mut registers = Registers::default();
        registers.pc = address;
        let mock = with_mapped_header(MockBackend::new(), fixture.path(), load_address)
            .with_thread(ThreadId(1), registers)
            .with_image(LoadedImage {
                path: executable.clone(),
                load_address: Address::new(load_address),
                build_id: ferros_core::types::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
            });
        let mut session = Session::new(
            Box::new(mock),
            MachOFormat::with_debug_info_options(scratch.options()).into_shared(),
        );
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");
        let assert_parse_failure = |error: DebuggerError| {
            let DebuggerError::SymbolLoad { path, source } = &error else {
                panic!("unexpected result {error}");
            };
            assert_eq!(path, &executable);
            assert!(
                matches!(&**source, DebuggerError::InvalidArgument(detail) if detail.contains("failed to parse")),
                "the original failure must be kept, not a new read's: {source}"
            );
        };

        assert_parse_failure(session.symbolicate(address).expect_err("corrupt image"));
        // Permissions are not part of the file's identity (size and timestamps), so the
        // kept failure still applies; reading the file now would fail differently.
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o000)).expect("make unreadable");
        for _ in 0..3 {
            assert_parse_failure(session.symbolicate(address).expect_err("kept failure"));
        }
        assert!(session.stack_trace(1).expect("raw stack")[0].symbol.is_none());
        assert_parse_failure(session.symbolicate(address).expect_err("kept failure after a stack trace"));

        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).expect("make readable");
        fs::remove_file(&executable).expect("remove corrupt image");
        fs::copy(fixture.path(), &executable).expect("replace with the real executable");
        std::os::unix::fs::symlink(fixture.dsym_path(), scratch.0.join("program.dSYM")).expect("link the dSYM");
        let symbolication = session
            .symbolicate(address)
            .expect("symbols once the file changed")
            .expect("main has a symbol");
        assert!(
            symbolication
                .frames
                .iter()
                .any(|frame| rustc_demangle::try_demangle(frame.symbol.raw())
                    .is_ok_and(|name| format!("{name:#}") == "ferros_fixture_hello::main")),
            "{symbolication:?}"
        );
    });
}

#[test]
fn session_symbolicates_the_replacement_build_at_the_same_address()
{
    harness::run(|_| {
        let first = fixture!("hello");
        let second = fixture!("nested_calls");
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        let dsym = scratch.0.join("program.dSYM");
        let load_address = Address::new(0x2_0000_0000);
        let loaded = |uuid: Option<[u8; 16]>| LoadedImage {
            path: executable.clone(),
            load_address,
            build_id: uuid.map(ferros_core::BuildId::from).into(),
            role: ferros_core::backend::ImageRole::MainExecutable,
        };
        let header_and_uuid = |fixture: &Fixture| {
            let mut bytes = fs::read(fixture.path()).expect("read executable");
            let uuid = object::File::parse(&*bytes)
                .expect("parse executable")
                .mach_uuid()
                .expect("read UUID")
                .expect("Mach-O has a UUID");
            bytes.truncate(ferros_macos::macho::header_size(&bytes).expect("header size"));
            (bytes, uuid)
        };
        let (mut header, first_uuid) = header_and_uuid(&first);
        let (replacement, second_uuid) = header_and_uuid(&second);
        assert_ne!(first_uuid, second_uuid);
        header.resize(header.len().max(replacement.len()), 0);

        // The first loader report omits the UUID, even though the mapped header has it.
        let mock = MockBackend::new()
            .with_region(load_address, "r-x", header)
            .with_thread(ThreadId(1), Registers::default())
            .with_image(loaded(None));
        let mut session = Session::new(
            Box::new(mock.clone()),
            MachOFormat::with_debug_info_options(scratch.options()).into_shared(),
        );
        let install = |fixture: &Fixture| {
            fs::copy(fixture.path(), &executable).expect("copy executable");
            std::os::unix::fs::symlink(fixture.dsym_path(), &dsym).expect("link matching dSYM");
        };
        let check = |session: &mut Session, fixture: &Fixture, function: &str, marker: &str| {
            let file = format!("{}.rs", fixture.name());
            let line = fixture.marker_line(marker);
            let image = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared())
                .load_image(descriptor(fixture.path(), load_address.value()))
                .expect("read fixture DWARF");
            let addresses = image.line_addresses(&file, line).expect("fixture marker addresses");
            assert_eq!(addresses.line, line);
            let address = addresses.addresses[0];
            let result = session.symbolicate(address).expect("symbolicate image");
            let result = result.unwrap_or_else(|| panic!("no DWARF result for {function} at {file}:{line}"));
            assert!(
                result.frames.iter().any(|frame| {
                    frame.symbol.display_name() == function
                        && frame
                            .location
                            .as_ref()
                            .is_some_and(|location| location.file.ends_with(&file) && location.line == Some(line))
                }),
                "expected {function} at line {line}, got {result:?}"
            );
            result.image_id
        };

        install(&first);
        session.launch(&LaunchSpec::new(&executable)).expect("mock launch");
        let first_id = check(&mut session, &first, "ferros_fixture_hello::main", "hello_print");

        fs::remove_file(&dsym).expect("unlink the old companion");
        install(&second);
        mock.overwrite_memory(load_address, &replacement);
        mock.set_images(vec![loaded(Some(second_uuid))]);
        let second_id = check(&mut session, &second, "ferros_fixture_nested_calls::level_c", "in_c");
        assert_ne!(first_id, second_id, "each build has its own DWARF cache identity");
    });
}

/// The target still maps build A of an executable while the file on disk is rebuilt to
/// build B. The session reads A's UUID from the mapped header and refuses to label B's
/// DWARF as A's: `symbolicate` reports a load failure naming another build, and once A's
/// file is back the next request reads it (#170).
#[test]
fn session_refuses_a_rebuilt_file_while_the_old_build_is_mapped()
{
    harness::run(|_| {
        let mapped = fixture!("hello");
        let rebuilt = fixture!("nested_calls");
        let scratch = Scratch::new();
        let executable = scratch.0.join("program");
        let dsym = scratch.0.join("program.dSYM");
        let load_address = Address::new(0x2_0000_0000);
        let mut header = fs::read(mapped.path()).expect("read executable");
        header.truncate(ferros_macos::macho::header_size(&header).expect("header size"));
        // The loader reports no UUID; the session reads it from the mapped header.
        let mock = MockBackend::new()
            .with_region(load_address, "r-x", header)
            .with_thread(ThreadId(1), Registers::default())
            .with_image(LoadedImage {
                path: executable.clone(),
                load_address,
                build_id: ferros_core::types::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
            });
        let mut session = Session::new(
            Box::new(mock),
            MachOFormat::with_debug_info_options(scratch.options()).into_shared(),
        );
        let install = |fixture: &Fixture| {
            let _ = fs::remove_file(&executable);
            let _ = fs::remove_file(&dsym);
            fs::copy(fixture.path(), &executable).expect("copy executable");
            std::os::unix::fs::symlink(fixture.dsym_path(), &dsym).expect("link matching dSYM");
        };
        let line = mapped.marker_line("hello_print");
        let address = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared())
            .load_image(descriptor(mapped.path(), load_address.value()))
            .expect("read fixture DWARF")
            .line_addresses("hello.rs", line)
            .expect("marker addresses")
            .addresses[0];

        install(&rebuilt);
        session.launch(&LaunchSpec::new(&executable)).expect("mock launch");
        match session.symbolicate(address) {
            Err(DebuggerError::SymbolLoad { path, source }) => {
                assert_eq!(path, executable);
                assert!(source.to_string().contains("another build"), "{source}");
            }
            other => panic!("the rebuilt file was read as the mapped build: {other:?}"),
        }

        install(&mapped);
        let result = session
            .symbolicate(address)
            .expect("the mapped build's file is back")
            .expect("a DWARF result");
        assert!(
            result
                .frames
                .iter()
                .any(|frame| frame.symbol.display_name() == "ferros_fixture_hello::main"),
            "{result:?}"
        );
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
            build_id: ferros_core::types::BuildIdentity::Unknown,
            role: ferros_core::backend::ImageRole::MainExecutable,
        };
        let second = LoadedImage {
            load_address: Address::new(0x3_0000_0000),
            ..first.clone()
        };
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let old_address = ferros_macos::macho::file_mapping(fixture.path(), first.load_address.value())
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mock = MockBackend::new();
        let mock = with_mapped_header(mock, fixture.path(), first.load_address.value());
        let mock = with_mapped_header(mock, fixture.path(), second.load_address.value())
            .with_thread(ThreadId(1), Registers::default())
            .with_image(first.clone());
        let mut session = Session::new(Box::new(mock.clone()), ferros_macos::MachOFormat::shared());
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
        let relative = relative_to_cwd(fixture.path());
        let image = LoadedImage {
            path: relative.clone(),
            load_address: Address::new(0x2_0000_0000),
            build_id: ferros_core::types::BuildIdentity::Unknown,
            role: ferros_core::backend::ImageRole::MainExecutable,
        };
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let address = ferros_macos::macho::file_mapping(&relative, image.load_address.value())
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mock = with_mapped_header(MockBackend::new(), fixture.path(), image.load_address.value())
            .with_thread(ThreadId(1), Registers::default())
            .with_image(image);
        let mut session = Session::new(Box::new(mock), ferros_macos::MachOFormat::shared());
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");
        assert!(session.symbolicate(address).expect("first lookup").is_some());
        assert!(session.symbolicate(address).expect("second lookup").is_some());
    });
}

/// Issue #99: an image the loader reports at a relative path keeps its loaded symbols
/// when that path stops resolving (its file is removed, as while Cargo re-uplifts a
/// fixture), as long as the image stays mapped. The session must never treat the symbols
/// as loaded while the cache has dropped them, which made lookups return `Ok(None)`.
#[test]
fn relative_image_keeps_symbols_after_its_path_stops_resolving()
{
    harness::run(|_| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let directory = fs::canonicalize(&scratch.0).expect("canonical scratch directory");
        let executable = directory.join("program");
        std::os::unix::fs::symlink(fixture.path(), &executable).expect("link the executable");
        std::os::unix::fs::symlink(fixture.dsym_path(), directory.join("program.dSYM")).expect("link the dSYM");
        let relative = relative_to_cwd(&executable);
        let load_address = 0x2_0000_0000;
        let (file, _) = symbol(fixture.path(), "ferros_fixture_hello::main");
        let address = ferros_macos::macho::file_mapping(fixture.path(), load_address)
            .expect("mapping")
            .runtime_address(file)
            .expect("runtime");
        let mock = with_mapped_header(MockBackend::new(), fixture.path(), load_address)
            .with_thread(ThreadId(1), Registers::default())
            .with_image(LoadedImage {
                path: relative.clone(),
                load_address: Address::new(load_address),
                build_id: ferros_core::types::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
            });
        let mut session = Session::new(
            Box::new(mock),
            MachOFormat::with_debug_info_options(scratch.options()).into_shared(),
        );
        session.launch(&LaunchSpec::new(fixture.path())).expect("mock launch");
        let is_main = |symbolication: &ferros_core::Symbolication| {
            symbolication.frames.iter().any(|frame| {
                rustc_demangle::try_demangle(frame.symbol.raw())
                    .is_ok_and(|name| format!("{name:#}") == "ferros_fixture_hello::main")
            })
        };
        let loaded = session
            .symbolicate(address)
            .expect("first lookup")
            .expect("main has a symbol");
        assert!(is_main(&loaded), "{loaded:?}");

        fs::remove_file(&executable).expect("remove the executable link");
        assert!(relative.canonicalize().is_err(), "the relative path must stop resolving");
        for _ in 0..2 {
            match session.symbolicate(address) {
                Ok(Some(symbolication)) => assert!(is_main(&symbolication), "{symbolication:?}"),
                Err(DebuggerError::SymbolLoad { path, .. }) => assert_eq!(path, relative),
                Ok(None) => panic!("symbols silently missing while the image is still mapped"),
                Err(error) => panic!("unexpected error {error}"),
            }
        }
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
            // dyld lists itself as well (issue #121); a library is anything else.
            assert!(
                images
                    .iter()
                    .any(|image| image.path != fixture.path() && image.path.file_name().is_none_or(|name| name != "dyld")),
                "dyld must enumerate libraries as well as the executable"
            );
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
        let mut session = ferros_macos::new_session().expect("backend");
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

/// Symbolicating dyld's entry at the launch stop skips generation when its mapped slice
/// has no debug map (#174).
#[test]
fn dyld_entry_skips_dsymutil_without_a_debug_map()
{
    harness::run(|t| {
        let fixture = fixture!("hello");
        let scratch = Scratch::new();
        let invocations = scratch.0.join("invocations");
        fs::write(&invocations, "").expect("create invocation log");
        let tool = scratch.0.join("record-dsymutil");
        fs::write(&tool, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${0%/*}/invocations\"\nexit 1\n")
            .expect("write tool script");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).expect("make tool executable");
        let mut options = scratch.options();
        options.dsymutil = Some(tool);

        let tag = t.tag();
        let mut session = ferros_macos::new_session()
            .expect("backend")
            .with_object_format(MachOFormat::with_debug_info_options(options).into_shared());
        session
            .launch(&LaunchSpec::new(fixture.path()).arg(tag.as_str()))
            .expect("launch at dyld entry");
        let entry = session.read_registers().expect("registers at launch stop").pc;
        let (owner, _) = session.file_address(entry).expect("file address").expect("entry is mapped");
        assert_eq!(owner, Path::new("/usr/lib/dyld"));
        let result = session.symbolicate(entry);
        session.kill().expect("kill and reap fixture");

        let invocations = fs::read_to_string(invocations).expect("read invocation log");
        assert!(invocations.is_empty(), "dsymutil was invoked with:\n{invocations}");
        let error = result.expect_err("dyld has no DWARF");
        assert!(
            matches!(&error, DebuggerError::NoDebugInfo { path, detail } if *path == owner && detail.contains("no debug map")),
            "{error}"
        );
    });
}

/// A Cargo-built fixture contains object-file stabs for dsymutil to link (#174).
#[test]
fn cargo_built_fixture_contains_debug_map_stabs()
{
    use object::read::macho::Nlist;

    harness::run(|_| {
        let fixture = fixture!("hello");
        let bytes = fs::read(fixture.path()).expect("read fixture");
        let object::File::MachO64(file) = object::File::parse(&*bytes).expect("parse fixture") else {
            panic!("expected a 64-bit Mach-O fixture");
        };
        let object_files = file
            .macho_symbol_table()
            .iter()
            .filter(|entry| entry.stab() == Some(object::macho::N_OSO))
            .count();
        assert!(object_files > 0, "fixture must have N_OSO stabs");
        println!("hello fixture N_OSO stabs: {object_files}");
    });
}

/// A launch of `frameless_stack` copied without its dSYM and with its debug map stripped
/// (`strip -S`), so no source debug information can be found for the main executable.
/// Its `__eh_frame`, `__unwind_info` and symbol table stay. The copy lives under the
/// test target directory, where `just kill-fixtures` finds a leaked process.
struct StrippedFixture
{
    directory: PathBuf,
    fixture: Fixture,
}

impl StrippedFixture
{
    fn new() -> Self
    {
        let built = fixture!("frameless_stack");
        let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "ferros-stripped-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create stripped fixture directory");
        let file_name = built.path().file_name().expect("fixture file name");
        let executable = directory.join(file_name);
        fs::copy(built.path(), &executable).expect("copy executable without its dSYM");
        let status = Command::new("strip").arg("-S").arg(&executable).status().expect("run strip");
        assert!(status.success(), "strip -S failed: {status}");
        // `strip` invalidates the ad-hoc signature arm64 macOS requires to run the copy.
        let status = Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&executable)
            .status()
            .expect("run codesign");
        assert!(status.success(), "codesign failed: {status}");
        let fixture = Fixture::from_built(
            "frameless_stack",
            Box::leak(executable.to_str().expect("UTF-8 path").to_owned().into_boxed_str()),
        );
        Self { directory, fixture }
    }
}

impl Drop for StrippedFixture
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// The first instruction at or after `from` that is `insn`, within 64 instructions.
fn instruction_at_or_after(session: &mut Session, from: Address, insn: u32) -> Address
{
    (0..64)
        .map(|index| Address::new(from.value() + 4 * index))
        .find(|&at| {
            let bytes = session.read_memory(at, 4).expect("read code");
            u32::from_le_bytes(bytes.try_into().expect("four bytes")) == insn
        })
        .unwrap_or_else(|| panic!("no instruction {insn:#010x} after {from}"))
}

fn stop_in_unwind_only_callee(d: &mut Session, fixture: &Fixture) -> (Address, Address, ThreadId)
{
    let leaf = ferros_tests::symbols::raw_symbol_address(d, fixture, "_unwind_only_callee");
    let outer = ferros_tests::symbols::raw_symbol_address(d, fixture, "_unwind_only_caller");
    let stop = instruction_at_or_after(d, leaf, 0xD503_201F); // nop
    let return_address = (0..16)
        .map(|index| Address::new(outer.value() + 4 * index))
        .find(|&at| {
            let bytes = d.read_memory(at, 4).expect("read code");
            u32::from_le_bytes(bytes.try_into().expect("four bytes")) & 0xFC00_0000 == 0x9400_0000
        })
        .map(|call| Address::new(call.value() + 4))
        .expect("the caller's `bl`");
    d.add_breakpoint(BreakpointRequest::Software { address: stop })
        .expect("break on the callee's nop");
    d.resume().expect("resume");
    let thread = match d.wait(Some(Duration::from_secs(10))).expect("wait") {
        Some(StopEvent::Breakpoint { pc, thread }) if pc == stop => thread,
        other => panic!("expected a breakpoint stop at {stop}, got {other:?}"),
    };
    d.select_thread(thread).expect("select the stopped thread");
    (stop, return_address, thread)
}

fn launch_with_debug_options(fixture: &Fixture, tag: &str, options: DebugInfoOptions) -> Session
{
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

/// `unwind_only_callee` of `frameless_stack` (see `fixtures/asm/frameless_stack.s`) has
/// only an `__eh_frame` FDE (compact unwind `UNWIND_ARM64_MODE_DWARF`). Stopped on its
/// `nop` the frame pointer is zero and the link register points at the callee itself, so
/// only that FDE finds the caller `unwind_only_caller`. The executable is run without a
/// dSYM or a debug map: it has no source debug information, but its unwind tables still
/// give the caller frame, at the return address the FDE's saved link register holds.
/// Symbolication there still reports the missing debug information, and step-out stops
/// in the caller.
#[test]
fn stripped_executable_still_unwinds_its_caller()
{
    harness::run(|t| {
        let stripped = StrippedFixture::new();
        let fixture = &stripped.fixture;
        assert!(!fixture.dsym_path().exists(), "the copy has no dSYM");
        let scratch = Scratch::new();
        let tag = t.tag();
        let mut d = launch_with_debug_options(fixture, tag.as_str(), scratch.options());
        let (stop, return_address, thread) = stop_in_unwind_only_callee(&mut d, fixture);

        let registers = d.read_registers_for(thread).expect("registers");
        assert_eq!(registers.fp, Address::ZERO, "the callee cleared the frame pointer");
        assert_ne!(
            registers.general.get(30).copied(),
            Some(return_address.value()),
            "the link register must not point at the caller"
        );
        let error = d.symbolicate(stop).expect_err("no source debug information");
        assert!(matches!(error, DebuggerError::NoDebugInfo { .. }), "{error}");

        let frames = d.stack_trace(8).expect("stack trace");
        assert_eq!(frames[0].pc, stop, "{frames:#?}");
        assert!(frames.len() >= 2, "the caller is missing: {frames:#?}");
        assert_eq!(
            frames[1].pc, return_address,
            "the caller is `unwind_only_caller` after its call: {frames:#?}"
        );
        assert_eq!(frames[1].sp.value(), frames[0].sp.value() + 16, "{frames:#?}");
        let error = d.symbolicate(stop).expect_err("still no source debug information");
        assert!(matches!(error, DebuggerError::NoDebugInfo { .. }), "{error}");

        d.step_out().expect("step out");
        match d.wait(Some(Duration::from_secs(10))).expect("wait") {
            Some(StopEvent::SingleStep { thread: stepped }) if stepped == thread => {}
            other => panic!("expected step-out to stop {thread:?}, got {other:?}"),
        }
        let registers = d.read_registers_for(thread).expect("registers");
        assert_eq!(registers.pc, return_address, "step-out stops in the caller");
    });
}

/// Keep the UUID, section names, and sizes valid for discovery, but make the
/// debug-info section unreadable when the engine extracts it.
fn corrupt_debug_info_offset(mut bytes: Vec<u8>) -> Vec<u8>
{
    let file = object::read::macho::MachOFile64::<object::Endianness>::parse(&*bytes).expect("parse dSYM");
    let section = file.section_by_name("__debug_info").expect("debug info");
    let offset = (&raw const section.macho_section().offset).addr() - bytes.as_ptr().addr();
    bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes
}

#[test]
fn failed_debug_retry_keeps_unwind_data_for_live_executable()
{
    harness::run(|t| {
        let stripped = StrippedFixture::new();
        let fixture = &stripped.fixture;
        let original = fixture!("frameless_stack");
        let directory = fixture.dsym_path().join("Contents/Resources/DWARF");
        fs::create_dir_all(&directory).expect("create companion directory");
        let member = fs::read_dir(original.dsym_path().join("Contents/Resources/DWARF"))
            .expect("fixture companion")
            .next()
            .expect("DWARF member")
            .expect("directory entry")
            .path();
        let original_dwarf = fs::read(member).expect("read fixture companion");
        let companion = directory.join("symbols");
        fs::write(&companion, corrupt_debug_info_offset(original_dwarf.clone())).expect("install malformed companion");

        let scratch = Scratch::new();
        let tag = t.tag();
        let mut d = launch_with_debug_options(fixture, tag.as_str(), scratch.options());
        let (stop, return_address, thread) = stop_in_unwind_only_callee(&mut d, fixture);

        let error = d.symbolicate(stop).expect_err("malformed companion");
        assert!(
            matches!(error, DebuggerError::SymbolLoad { ref source, .. } if source.to_string().contains("failed to read .debug_info")),
            "{error}"
        );
        let frames = d.stack_trace(2).expect("stack trace with a malformed companion");
        assert_eq!(frames.get(1).map(|frame| frame.pc), Some(return_address), "{frames:#?}");

        let moved = stripped.directory.join("moved");
        fs::rename(fixture.path(), &moved).expect("remove executable from its original path");
        let frames = d.stack_trace(2).expect("stack trace after a failed executable reopen");
        assert_eq!(
            frames.get(1).map(|frame| frame.pc),
            Some(return_address),
            "failed reopen lost the caller: {frames:#?}"
        );

        // Restoring the executable changes its stamp again. Let that reopen finish before
        // changing the companion alone.
        fs::rename(&moved, fixture.path()).expect("restore executable");
        assert!(matches!(d.symbolicate(stop), Err(DebuggerError::SymbolLoad { .. })));

        // Permissions are outside the stamp. Only the companion changes, so recovery
        // must work even though reading the executable would now fail.
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o000)).expect("make executable unreadable");
        assert!(fs::read(fixture.path()).is_err(), "executable is still readable");
        let replacement = stripped.directory.join("replacement");
        fs::write(&replacement, original_dwarf).expect("write the valid companion");
        fs::rename(replacement, &companion).expect("replace the malformed companion");

        let line = original.marker_line("frameless_done");
        d.set_breakpoint(BreakpointLocation::Line {
            file: "frameless_stack.rs".into(),
            line,
        })
        .expect("reload DWARF independently of the executable");
        let site = &d.logical_breakpoints()[0].sites[0];
        assert_eq!(site.position.line, Some(line));
        let address = site.address;
        assert!(d.symbolicate(address).expect("symbolicate main").is_some());

        d.step_out().expect("step out");
        assert!(matches!(
            d.wait(Some(Duration::from_secs(10))).expect("wait"),
            Some(StopEvent::SingleStep { thread: stepped }) if stepped == thread
        ));
        assert_eq!(d.read_registers_for(thread).expect("registers").pc, return_address);
    });
}

#[test]
fn failed_public_loads_leave_symbol_cache_untouched()
{
    harness::run(|_| {
        let stripped = StrippedFixture::new();
        let fixture = fixture!("frameless_stack");
        let scratch = Scratch::new();
        let stripped_load = Address::new(0x2_0000_0000);
        let full_load = stripped_load.value() + 0x4000;
        let stripped_mapping =
            ferros_macos::macho::file_mapping(stripped.fixture.path(), stripped_load.value()).expect("stripped mapping");
        let slice = ferros_macos::macho::SliceIdentity::new(
            object::macho::CPU_TYPE_ARM64.0,
            object::macho::CPU_SUBTYPE_ARM64_ALL.0,
            None,
        )
        .to_image_slice();
        let observed = [None, Some(&slice)].map(|slice| {
            let mut cache = SymbolCache::new(MachOFormat::with_debug_info_options(scratch.options()).into_shared());
            let full = cache.load_image(descriptor(fixture.path(), full_load)).expect("full DWARF");
            let line = fixture.marker_line("frameless_done");
            let address = full
                .line_addresses("frameless_stack.rs", line)
                .expect("main marker")
                .addresses[0];
            assert!(stripped_mapping.contains(address), "the mappings must overlap at the marker");
            assert!(stripped_mapping.contains(stripped_load));
            assert!(!full.contains(stripped_load));

            let descriptor = descriptor(stripped.fixture.path(), stripped_load.value());
            let result = match slice {
                Some(slice) => cache.load_image_slice(descriptor, slice),
                None => cache.load_image(descriptor),
            };
            expect_no_debug_info(result);

            // This address belongs only to the stripped image. It detects an insertion
            // regardless of which image overlapping-address lookup would find first.
            (
                cache.image_for_address(stripped_load).is_none(),
                cache.symbolicate(address).is_some_and(|result| result.image_id == full.id()),
            )
        });
        assert_eq!(observed, [(true, true); 2], "failed loads changed the cache");
    });
}
