//! A fixture copied without its dSYM and with its debug map stripped, for tests of images
//! without source debug information (issue #236).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Fixture;

/// A copy of a built fixture without its dSYM and with its debug map stripped
/// (`strip -S`), so no source debug information can be found for it. Its `__eh_frame`,
/// `__unwind_info` and symbol table stay. The copy is ad-hoc signed again so it runs.
///
/// The copy lives in a fresh directory under `parent`; pass the test target directory
/// (`env!("CARGO_TARGET_TMPDIR")`), where `just kill-fixtures` finds a leaked process.
/// Dropping the value removes that directory.
pub struct StrippedFixture
{
    /// The directory holding the copy, removed on drop. Tests may put more files in it.
    pub directory: PathBuf,
    /// The stripped copy, under the built fixture's name.
    pub fixture: Fixture,
}

impl StrippedFixture
{
    /// Copy `built` into a new directory under `parent` and strip it.
    ///
    /// # Panics
    /// Panics if the copy, `strip` or `codesign` fails, or the copy's path is not UTF-8.
    #[must_use]
    pub fn new(built: &Fixture, parent: &Path) -> Self
    {
        let directory = unique_dir(parent, &format!("ferros-stripped-{}", built.name()));
        let executable = directory.join(built.path().file_name().expect("fixture file name"));
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
        let name: &'static str = Box::leak(built.name().to_owned().into_boxed_str());
        let fixture = Fixture::from_built(
            name,
            Box::leak(executable.to_str().expect("UTF-8 path").to_owned().into_boxed_str()),
        );
        assert!(!fixture.dsym_path().exists(), "the copy has no dSYM");
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

/// A new directory `<parent>/<stem>-<pid>-<n>`, retrying on a name already taken.
fn unique_dir(parent: &Path, stem: &str) -> PathBuf
{
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(parent).expect("create the parent of the stripped fixture directory");
    loop {
        let count = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{stem}-{}-{count}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create {}: {error}", path.display()),
        }
    }
}
