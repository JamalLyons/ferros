//! Unique temporary paths for tests.
//!
//! A name built from the pid plus a clock reading can repeat: two threads can read the same
//! nanosecond, and two test processes can share a pid after one exits. Every name here
//! combines the pid with a process-wide counter, and creation retries with a fresh name on
//! `AlreadyExists` so a stale leftover from a reused pid cannot fail a test either.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn candidate(stem: &str, suffix: &str) -> PathBuf
{
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = stem.to_owned();
    name.push('-');
    name.push_str(&std::process::id().to_string());
    name.push('-');
    name.push_str(&count.to_string());
    name.push_str(suffix);
    std::env::temp_dir().join(name)
}

/// Creates a new empty file `<temp>/<stem>-<pid>-<n><suffix>` and returns it with its path.
///
/// # Panics
/// Panics if the file cannot be created.
#[must_use]
pub fn create_file(stem: &str, suffix: &str) -> (File, PathBuf)
{
    loop {
        let path = candidate(stem, suffix);
        match File::create_new(&path) {
            Ok(file) => return (file, path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create {}: {error}", path.display()),
        }
    }
}

/// Creates a new empty directory `<temp>/<stem>-<pid>-<n>` and returns its path.
///
/// # Panics
/// Panics if the directory cannot be created.
#[must_use]
pub fn create_dir(stem: &str) -> PathBuf
{
    loop {
        let path = candidate(stem, "");
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create {}: {error}", path.display()),
        }
    }
}

/// Returns an unused path `<temp>/<stem>-<pid>-<n><suffix>` without creating it.
///
/// Prefer [`create_file`] or [`create_dir`] when the test creates the entry itself; this is
/// for names the test hands to code that creates them, such as a copy destination.
#[must_use]
pub fn unique_path(stem: &str, suffix: &str) -> PathBuf
{
    loop {
        let path = candidate(stem, suffix);
        if !path.exists() {
            return path;
        }
    }
}
