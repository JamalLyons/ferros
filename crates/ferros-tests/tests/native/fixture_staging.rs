//! Stable fixture copies (issue #84).
//!
//! Every Cargo command removes and recreates the uplifted `target/debug/ferros-fixture-*`
//! files, so a test that reads or launches them while another Cargo command runs can find
//! them missing. `fixture!` hands out a private copy taken once per test process instead.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ferros_tests::symbols::{function_address, marker_address};
use ferros_tests::{Fixture, fixture, harness, scratch};

struct Scratch(PathBuf);

impl Scratch
{
    fn new() -> Self
    {
        let path = scratch::create_dir("ferros-staging");
        Self(fs::canonicalize(&path).expect("canonical scratch directory"))
    }
}

impl Drop for Scratch
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(source: &Path, destination: &Path)
{
    fs::create_dir(destination).expect("create directory");
    for entry in fs::read_dir(source).expect("list directory") {
        let from = entry.expect("directory entry").path();
        let to = destination.join(from.file_name().expect("file name"));
        if fs::metadata(&from).expect("metadata").is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

fn leak(path: &Path) -> &'static str
{
    Box::leak(path.to_str().expect("UTF-8 path").into())
}

/// `fixture!` does not hand out Cargo's uplifted path, but a copy under the test crate's
/// `CARGO_TARGET_TMPDIR` with the same file name and a dSYM next to it.
#[test]
fn fixture_macro_uses_a_stable_copy()
{
    let fixture = fixture!("hello");
    let uplifted = Path::new(env!("CARGO_BIN_EXE_ferros-fixture-hello"));
    let root = fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).expect("canonical target tmpdir");
    assert_ne!(fixture.path(), uplifted);
    assert!(fixture.path().starts_with(&root), "{}", fixture.path().display());
    assert_eq!(fixture.path().file_name(), uplifted.file_name());
    assert!(fixture.path().is_file());
    assert!(fixture.dsym_path().join("Contents/Resources/DWARF").is_dir());
    assert_eq!(fixture!("hello").path(), fixture.path(), "staged once per process");
}

/// Once staged, a fixture is read and launched without the built files: removing them,
/// as a concurrent Cargo command does for a moment, changes nothing. The loader reports
/// the copy's path, so `Fixture::path` comparisons still hold.
#[test]
fn staged_fixture_survives_removal_of_the_built_files()
{
    harness::run(|t| {
        let scratch = Scratch::new();
        let built = scratch.0.join("built");
        fs::create_dir(&built).expect("create built directory");
        let executable = built.join("ferros-fixture-hello");
        let original = fixture!("hello");
        fs::copy(original.path(), &executable).expect("copy the executable");
        copy_tree(&original.dsym_path(), &built.join("ferros-fixture-hello.dSYM"));

        let fixture = Fixture::stable("hello", leak(&executable), leak(&scratch.0.join("tmp")));
        fs::remove_dir_all(&built).expect("remove the built files");
        assert!(fixture.path().is_file(), "{}", fixture.path().display());

        let mut d = t.launch(&fixture);
        let images = d.images().expect("images");
        assert!(images.iter().any(|image| image.path == fixture.path()), "{images:?}");
        let main = function_address(&mut d, &fixture, "main");
        let marker = marker_address(&mut d, &fixture, "hello_print");
        assert!(main.value() <= marker.value(), "main {main} after hello_print {marker}");
        d.resume().expect("resume");
        assert!(d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)));
    });
}

/// A line-only rebuild can keep the executable UUID while changing its dSYM. Never
/// reuse the old source locations just because the machine code did not change.
#[test]
fn same_executable_with_new_dwarf_gets_a_new_stable_copy()
{
    harness::run(|_| {
        let scratch = Scratch::new();
        let original = fixture!("hello");
        let root = leak(&scratch.0.join("tmp"));
        let mut copies = Vec::new();
        for version in ["before", "after"] {
            let built = scratch.0.join(version);
            fs::create_dir(&built).expect("create built directory");
            let executable = built.join("ferros-fixture-hello");
            fs::copy(original.path(), &executable).expect("copy executable");
            let dsym = built.join("ferros-fixture-hello.dSYM");
            copy_tree(&original.dsym_path(), &dsym);
            if version == "after" {
                // Change a source filename without changing the Mach-O UUID, just as
                // line-table-only changes leave the executable's machine code intact.
                let members = dsym.join("Contents/Resources/DWARF");
                let member = fs::read_dir(members)
                    .expect("DWARF members")
                    .next()
                    .expect("member")
                    .expect("entry")
                    .path();
                let mut bytes = fs::read(&member).expect("read DWARF");
                let offset = bytes
                    .windows(b"hello.rs".len())
                    .position(|bytes| bytes == b"hello.rs")
                    .expect("source filename");
                bytes[offset] = b'j';
                fs::write(member, bytes).expect("write updated DWARF");
            }
            copies.push(Fixture::stable("hello", leak(&executable), root));
        }
        assert_ne!(
            copies[0].path(),
            copies[1].path(),
            "new DWARF must not reuse stale source locations"
        );
        assert_eq!(
            fs::read(copies[0].path()).expect("old executable"),
            fs::read(copies[1].path()).expect("new executable")
        );
        let dwarf = |fixture: &Fixture| {
            let member = fs::read_dir(fixture.dsym_path().join("Contents/Resources/DWARF"))
                .expect("members")
                .next()
                .expect("member")
                .expect("entry");
            fs::read(member.path()).expect("read staged DWARF")
        };
        assert_ne!(dwarf(&copies[0]), dwarf(&copies[1]));
    });
}
