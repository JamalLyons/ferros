use std::fs::{self, File};
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ferros_core::{DebuggerError, ProcessId, Session};

use super::process::process_is_gone;
use super::tracking::{Shared, Tracked};

/// A launched fixture process together with the [`Session`] debugging it.
///
/// Dereferences to [`Session`], so session methods can be called directly
/// (`debuggee.read_registers()`). Dropping it drops the session first, letting its ownership
/// policy terminate and reap a launched target. A tag sweep catches a detached target;
/// no stale pid is ever signalled. Output readers and capture files are then removed.
/// Cleanup logs any process that remains and never panics.
pub struct Debuggee
{
    pub(super) session: Session,
    pub(super) pid: ProcessId,
    pub(super) stdout: OutputBuffer,
    pub(super) stderr: OutputBuffer,
    pub(super) entry: Arc<Tracked>,
    pub(super) shared: Arc<Shared>,
}

impl Debuggee
{
    /// Process id of the debuggee.
    #[must_use]
    pub fn pid(&self) -> ProcessId
    {
        self.pid
    }

    /// The identifying argument appended to the debuggee's argv (`--ferros-test-id=...`).
    #[must_use]
    pub fn tag(&self) -> &str
    {
        &self.entry.tag
    }

    /// The session debugging this process.
    #[must_use]
    pub fn session(&self) -> &Session
    {
        &self.session
    }

    /// Mutable access to the session debugging this process.
    #[must_use]
    pub fn session_mut(&mut self) -> &mut Session
    {
        &mut self.session
    }

    /// Everything the debuggee wrote to stdout so far (lossy UTF-8).
    #[must_use]
    pub fn stdout(&self) -> String
    {
        self.stdout.snapshot()
    }

    /// Everything the debuggee wrote to stderr so far (lossy UTF-8).
    #[must_use]
    pub fn stderr(&self) -> String
    {
        self.stderr.snapshot()
    }

    /// Waits up to `timeout` for the captured stdout to contain `needle`.
    ///
    /// Returns `true` as soon as it does, `false` when the timeout passes first.
    #[must_use]
    pub fn wait_for_stdout(&self, needle: &str, timeout: Duration) -> bool
    {
        let deadline = Instant::now() + timeout;
        loop {
            if self.stdout().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Deref for Debuggee
{
    type Target = Session;

    fn deref(&self) -> &Self::Target
    {
        &self.session
    }
}

impl DerefMut for Debuggee
{
    fn deref_mut(&mut self) -> &mut Self::Target
    {
        &mut self.session
    }
}

impl Drop for Debuggee
{
    fn drop(&mut self)
    {
        let empty = ferros_macos::new_session().expect("empty session");
        drop(std::mem::replace(&mut self.session, empty));
        self.shared.finish(&self.entry);
        if !process_is_gone(self.pid.0) {
            eprintln!("ferros-tests: cleanup left pid {} present", self.pid.0);
        }
    }
}

/// Per-launch temporary directory holding the debuggee's stdout and stderr files.
pub(super) struct CaptureDir
{
    pub(super) path: PathBuf,
}

impl CaptureDir
{
    /// Creates a fresh directory under the system temporary directory, named after `tag`,
    /// with two empty files for stdout and stderr.
    pub(super) fn create(tag: &str) -> Result<Self, DebuggerError>
    {
        let name = tag.trim_start_matches("--").replace('=', "-");
        let path = std::env::temp_dir().join(format!("ferros-tests-{name}"));
        fs::create_dir_all(&path)?;
        let dir = Self { path };
        File::create(dir.stdout_path())?;
        File::create(dir.stderr_path())?;
        Ok(dir)
    }

    pub(super) fn stdout_path(&self) -> PathBuf
    {
        self.path.join("stdout")
    }

    pub(super) fn stderr_path(&self) -> PathBuf
    {
        self.path.join("stderr")
    }

    pub(super) fn remove(&self)
    {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Drop for CaptureDir
{
    fn drop(&mut self)
    {
        // Covers the failed-launch paths, where no `Debuggee` is built.
        self.remove();
    }
}

pub(super) struct Capture
{
    pub(super) dir: CaptureDir,
    pub(super) stdout: Option<OutputReader>,
    pub(super) stderr: Option<OutputReader>,
}

/// How often an output reader looks for new data after reaching the end of its file.
const TAIL_INTERVAL: Duration = Duration::from_millis(10);

/// Captured output of a debuggee, filled by a background thread that tails a file.
pub(super) struct OutputBuffer
{
    pub(super) bytes: Arc<Mutex<Vec<u8>>>,
}

impl OutputBuffer
{
    /// Starts a thread that follows `path` until its reader guard is dropped.
    pub(super) fn spawn_reader(path: PathBuf) -> (Self, OutputReader)
    {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let sink = Arc::clone(&bytes);
        let stop_flag = Arc::clone(&stop);
        let reader = thread::Builder::new()
            .name("ferros-output-reader".to_owned())
            .spawn(move || tail_file(&path, &sink, &stop_flag))
            .ok();
        (Self { bytes }, OutputReader { stop, reader })
    }

    pub(super) fn snapshot(&self) -> String
    {
        String::from_utf8_lossy(&self.bytes.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

/// Control of a reader, kept in the shared capture so the controller can join it.
pub(super) struct OutputReader
{
    pub(super) stop: Arc<AtomicBool>,
    pub(super) reader: Option<thread::JoinHandle<()>>,
}

impl Drop for OutputReader
{
    fn drop(&mut self)
    {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            reader.thread().unpark();
            let _ = reader.join();
        }
    }
}

/// Appends everything written to `path` to `sink` until `stop` is set, then reads once more.
fn tail_file(path: &Path, sink: &Mutex<Vec<u8>>, stop: &AtomicBool)
{
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let mut chunk = vec![0_u8; 4096];
    loop {
        // Check between reads so a writer still appending data cannot delay shutdown.
        let stopping = stop.load(Ordering::SeqCst);
        let at_eof = match file.read(&mut chunk) {
            Ok(0) => true,
            Ok(read) => {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&chunk[..read]);
                false
            }
            Err(_) => return,
        };
        if stopping {
            return;
        }
        if at_eof {
            thread::park_timeout(TAIL_INTERVAL);
        }
    }
}
