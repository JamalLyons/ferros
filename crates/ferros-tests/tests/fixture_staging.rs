//! Stable fixture copies (issue #84).
//!
//! Every Cargo command removes and recreates the uplifted `target/debug/ferros-fixture-*`
//! files, so a test that reads or launches them while another Cargo command runs can find
//! them missing. `fixture!` hands out a private copy taken once per test process instead.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ferros_tests::symbols::{function_address, marker_address};
use ferros_tests::{Fixture, fixture, harness};

struct Scratch(PathBuf);

impl Scratch
{
    fn new() -> Self
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferros-staging-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create scratch directory");
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
