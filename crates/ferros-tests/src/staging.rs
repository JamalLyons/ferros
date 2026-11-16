//! Stable copies of fixture executables and their dSYMs (issue #84).
//!
//! Every Cargo command, even one with nothing to rebuild, removes and recreates each uplifted
//! `target/debug/ferros-fixture-*` file and its `.dSYM` link. A test that reads or launches the
//! uplifted path while another Cargo command runs can find it missing. So each test process
//! copies a fixture once into Cargo's per-target temporary directory, which Cargo never
//! rewrites, and uses that copy for the rest of the run.
//!
//! A copy lives in `<root>/ferros-fixtures/<binary>-<Mach-O UUID>-<DWARF hash>/`, next to
//! its dSYM under the original file name. Line-only rebuilds can keep the executable
//! UUID, so the DWARF content also identifies the build. Identical builds share a copy. A
//! directory is published by renaming a complete private staging directory, so no process
//! sees a partial copy. Copies are removed by `cargo clean`.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use std::{fs, io, process, thread};

use object::Object;

/// How long a fixture may stay missing (or its dSYM from another build) before staging
/// fails. Cargo replaces the uplifted file within milliseconds; only a missing or
/// rebuilding fixture takes longer.
const MISSING_WAIT: Duration = Duration::from_secs(10);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Returns the stable copy of `built` under `root`, making it on the first call for `built`
/// in this process.
///
/// # Panics
///
/// Panics when the copy cannot be made: `built` stays missing for [`MISSING_WAIT`], is not
/// a Mach-O file with a UUID, has no dSYM from the same build, or a file system operation
/// fails.
pub(crate) fn staged(built: &Path, root: &Path) -> PathBuf
{
    static STAGED: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

    let mut staged = STAGED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(path) = staged.get(built) {
        return path.clone();
    }
    let path =
        stage(built, root).unwrap_or_else(|error| panic!("stage a stable copy of fixture {}: {error}", built.display()));
    staged.insert(built.to_path_buf(), path.clone());
    path
}

/// Why one staging attempt did not produce a copy.
enum Failure
{
    /// The uplifted files are being replaced; a later attempt can succeed.
    Transient(String),
    /// Retrying cannot help.
    Fatal(String),
}

fn stage(built: &Path, root: &Path) -> Result<PathBuf, String>
{
    let file_name = built
        .file_name()
        .ok_or_else(|| format!("{} has no file name", built.display()))?;
    let directory = root.join("ferros-fixtures");
    fs::create_dir_all(&directory).map_err(|error| format!("create {}: {error}", directory.display()))?;
    // Loader-reported image paths are compared with `Fixture::path`, so use no symlinks.
    let directory = fs::canonicalize(&directory).map_err(|error| format!("resolve {}: {error}", directory.display()))?;

    let deadline = Instant::now() + MISSING_WAIT;
    loop {
        match attempt(built, file_name, &directory) {
            Ok(path) => return Ok(path),
            Err(Failure::Fatal(error)) => return Err(error),
            Err(Failure::Transient(error)) if Instant::now() >= deadline => {
                return Err(format!("{error} (still after {MISSING_WAIT:?})"));
            }
            Err(Failure::Transient(_)) => thread::sleep(RETRY_INTERVAL),
        }
    }
}

fn attempt(built: &Path, file_name: &OsStr, directory: &Path) -> Result<PathBuf, Failure>
{
    let staging = Staging::create(directory)?;
    let executable = staging.0.join(file_name);
    fs::copy(built, &executable).map_err(|error| io_failure("copy", built, &error))?;
    let bytes = fs::read(&executable).map_err(|error| io_failure("read", &executable, &error))?;
    let uuid = mach_uuid(&bytes).map_err(|error| Failure::Fatal(format!("{}: {error}", built.display())))?;
    let built_dsym = with_suffix(built, ".dSYM");
    let staged_dsym = with_suffix(&executable, ".dSYM");
    copy_tree(&built_dsym, &staged_dsym)?;
    let Some(dwarf_hash) = dwarf_fingerprint(&staged_dsym, uuid)? else {
        // Cargo rebuilt the fixture between the two copies.
        return Err(Failure::Transient(format!(
            "{} has no DWARF member for UUID {}",
            built_dsym.display(),
            hex(uuid)
        )));
    };
    let mut published_name = file_name.to_os_string();
    published_name.push(format!("-{}-{dwarf_hash:016x}", hex(uuid)));
    let published = directory.join(published_name);
    if published.join(file_name).is_file() {
        // Reuse only when both executable identity and source locations match.
        return Ok(published.join(file_name));
    }

    match fs::rename(&staging.0, &published) {
        Ok(()) => staging.disarm(),
        // Another test process published the same build first; its copy is identical.
        Err(_) if published.join(file_name).is_file() => {}
        Err(error) => return Err(io_failure("publish", &published, &error)),
    }
    Ok(published.join(file_name))
}

/// A private directory that is removed unless it was published.
struct Staging(PathBuf);

impl Staging
{
    fn create(directory: &Path) -> Result<Self, Failure>
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = directory.join(format!(".staging-{}-{}", process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Failure::Fatal(format!("create {}: {error}", path.display()))),
            }
        }
    }

    fn disarm(self)
    {
        std::mem::forget(self);
    }
}

impl Drop for Staging
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A missing source is Cargo replacing it; anything else is fatal.
fn io_failure(action: &str, path: &Path, error: &io::Error) -> Failure
{
    let message = format!("{action} {}: {error}", path.display());
    if error.kind() == ErrorKind::NotFound {
        Failure::Transient(message)
    } else {
        Failure::Fatal(message)
    }
}

/// Copies the directory `source` (following symlinks, as the uplifted `.dSYM` is one).
fn copy_tree(source: &Path, destination: &Path) -> Result<(), Failure>
{
    fs::create_dir(destination).map_err(|error| io_failure("create", destination, &error))?;
    let entries = fs::read_dir(source).map_err(|error| io_failure("list", source, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_failure("list", source, &error))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::metadata(&from).map_err(|error| io_failure("inspect", &from, &error))?;
        if metadata.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|error| io_failure("copy", &from, &error))?;
        }
    }
    Ok(())
}

fn dwarf_fingerprint(dsym: &Path, uuid: [u8; 16]) -> Result<Option<u64>, Failure>
{
    let members = dsym.join("Contents/Resources/DWARF");
    let entries = fs::read_dir(&members).map_err(|error| io_failure("list", &members, &error))?;
    for entry in entries {
        let path = entry.map_err(|error| io_failure("list", &members, &error))?.path();
        let bytes = fs::read(&path).map_err(|error| io_failure("read", &path, &error))?;
        if mach_uuid(&bytes).is_ok_and(|member| member == uuid) {
            let mut hash = DefaultHasher::new();
            bytes.hash(&mut hash);
            return Ok(Some(hash.finish()));
        }
    }
    Ok(None)
}

fn mach_uuid(bytes: &[u8]) -> Result<[u8; 16], String>
{
    let file = object::File::parse(bytes).map_err(|error| format!("parse: {error}"))?;
    file.mach_uuid()
        .map_err(|error| format!("read UUID: {error}"))?
        .ok_or_else(|| "no Mach-O UUID".to_owned())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf
{
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn hex(bytes: [u8; 16]) -> String
{
    bytes.iter().fold(String::with_capacity(32), |mut text, byte| {
        write!(text, "{byte:02x}").expect("write to a String");
        text
    })
}
