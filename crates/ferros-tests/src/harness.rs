//! Harness for integration tests that debug real fixture processes (macOS only).
//!
//! Every integration test body runs under [`run`] (or [`run_with_timeout`]), which gives it a
//! [`TestContext`]. The context launches fixtures under a fresh [`Session`] (driving a
//! `ferros_macos::MacOSBackend`) and returns a [`Debuggee`] guard that owns the session and
//! kills the debuggee when dropped.
//!
//! ```ignore
//! use ferros_core::SessionState;
//! use ferros_tests::{fixture, harness};
//!
//! harness::run(|t| {
//!     let mut d = t.launch(&fixture!("hello"));
//!     assert_eq!(d.state(), SessionState::Stopped);
//!     d.resume().expect("resume");
//!     assert!(d.wait_for_stdout("hello from ferros fixture", std::time::Duration::from_secs(10)));
//! });
//! ```
//!
//! Guarantees:
//!
//! - **No leaked processes.** Each launch appends a unique `--ferros-test-id=...` argument to
//!   the fixture's argv. Dropping the [`Debuggee`] (normal return or unwinding) drops its
//!   session first, then sweeps processes by that argument; a failed launch kills any
//!   process left behind by the argument; a timeout kills every process the test launched.
//!   Cleanup also reaps owned children, joins output readers and removes capture directories.
//! - **Hard deadline.** The body runs on a worker thread. If it does not finish in time the
//!   test fails with a "timed out" message after its debuggees are killed. Only that test's
//!   processes are touched, so tests can run in parallel.
//! - **Fast permission failures.** If `task_for_pid` is denied, or `Session::launch` does not
//!   return within [`LAUNCH_DEADLINE`] (for example because the system is waiting on an
//!   authorization dialog), the test fails with guidance on signing the test executable.
//! - **Panics stay panics.** A panic in the body reaches libtest with its original payload.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferros_core::backend::StdioSpec;
use ferros_core::{DebuggerError, LaunchSpec, ProcessId, Session};

use crate::{Fixture, scratch};

/// Deadline applied by [`run`] to a whole test body.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`TestContext::launch`] waits for `Session::launch` to return before it treats
/// the launch as blocked on a permission prompt and fails the test.
pub const LAUNCH_DEADLINE: Duration = Duration::from_secs(5);

/// Prefix of the identifying argument appended as the last argv entry of every launched
/// fixture. `pgrep -f -- --ferros-test-id=` lists fixtures started by this harness.
pub const TEST_ID_ARG_PREFIX: &str = "--ferros-test-id=";

/// Entitlement the test executable needs to call `task_for_pid` without root.
pub const DEBUGGER_ENTITLEMENT: &str = "com.apple.security.cs.debugger";

/// How long cleanup waits for a killed process to disappear before giving up.
const KILL_SETTLE: Duration = Duration::from_secs(2);

/// After a launch stalled, how often and for how long its watchdog keeps looking for the
/// half-launched process while `Session::launch` has still not returned.
const STALL_SWEEP_INTERVAL: Duration = Duration::from_millis(200);
const STALL_SWEEP_LIMIT: Duration = Duration::from_secs(30);

/// Runs a test body under the [`DEFAULT_TIMEOUT`] deadline.
///
/// See [`run_with_timeout`].
///
/// # Panics
///
/// Panics (failing the test) when the body panics, when the deadline passes, or when a
/// launch hits a permission failure.
pub fn run<F>(body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    run_with_timeout(DEFAULT_TIMEOUT, body);
}

/// Runs a test body on a worker thread with a hard deadline.
///
/// The body receives a [`TestContext`] for launching fixtures. When the body returns, any
/// process it launched that is still alive is killed. When the body panics, its panic is
/// resumed on the calling thread with the original payload, so `#[should_panic]` and the
/// libtest failure message work as usual. When `timeout` passes first, every process launched
/// through this context is killed and the test fails with a message that says it timed out;
/// the worker thread is abandoned. The controller joins output readers and gives process
/// cleanup one shared settle window.
///
/// # Panics
///
/// Panics (failing the test) when the body panics, when the deadline passes, when a launch
/// hits a permission failure, or when the worker thread cannot be spawned.
pub fn run_with_timeout<F>(timeout: Duration, body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    let (events_tx, events_rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    let context = TestContext {
        shared: Arc::clone(&shared),
        events: events_tx.clone(),
    };

    // Name the worker after the test so panic messages point at the right test.
    let name = thread::current().name().unwrap_or("ferros-test").to_owned();
    let spawned = thread::Builder::new().name(name).spawn(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(|| body(&context)));
        drop(context);
        let _ = events_tx.send(Event::Finished(result));
    });
    if let Err(err) = spawned {
        panic!("ferros-tests: failed to spawn the test worker thread: {err}");
    }

    let deadline = Instant::now() + timeout;
    let outcome = match events_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Event::Finished(result)) => Outcome::Finished(result),
        Ok(Event::LaunchStalled) => Outcome::LaunchStalled,
        Err(RecvTimeoutError::Timeout) => Outcome::TimedOut,
        Err(RecvTimeoutError::Disconnected) => {
            Outcome::Finished(Err(Box::new("ferros-tests: the test worker thread vanished")))
        }
    };

    if !matches!(outcome, Outcome::Finished(_)) {
        shared.cancelled.store(true, Ordering::SeqCst);
    }
    let killed = shared.kill_all();
    shared.remove_scratch();

    match outcome {
        Outcome::Finished(Ok(())) => {}
        Outcome::Finished(Err(payload)) => panic::resume_unwind(payload),
        Outcome::LaunchStalled => panic!("{}", launch_stalled_guidance()),
        Outcome::TimedOut => {
            panic!("ferros-tests: test timed out after {timeout:?}; killed its debuggee processes {killed:?}")
        }
    }
}

enum Event
{
    Finished(thread::Result<()>),
    LaunchStalled,
}

enum Outcome
{
    Finished(thread::Result<()>),
    LaunchStalled,
    TimedOut,
}

struct Tracked
{
    tag: String,
    process: ProcessCleanup,
    capture: Mutex<Option<Capture>>,
}

enum ProcessCleanup
{
    Tagged,
    Spawned(Mutex<Option<Child>>),
    Launched(ferros_macos::launch::ChildCleanup),
}

impl Tracked
{
    fn spawned_child(&self) -> MutexGuard<'_, Option<Child>>
    {
        let ProcessCleanup::Spawned(child) = &self.process else {
            unreachable!("only a spawned fixture has a Child handle");
        };
        child.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start_readers(&self) -> Option<[OutputBuffer; 2]>
    {
        let mut capture = self.capture.lock().unwrap_or_else(PoisonError::into_inner);
        let capture = capture.as_mut()?;
        let (stdout, stdout_reader) = OutputBuffer::spawn_reader(capture.dir.stdout_path());
        let (stderr, stderr_reader) = OutputBuffer::spawn_reader(capture.dir.stderr_path());
        capture.stdout = Some(stdout_reader);
        capture.stderr = Some(stderr_reader);
        Some([stdout, stderr])
    }

    fn close_capture(&self)
    {
        let mut capture = self.capture.lock().unwrap_or_else(PoisonError::into_inner);
        // Keep the lock through the joins so another cleanup cannot return early.
        if let Some(Capture { dir, stdout, stderr }) = capture.take() {
            drop(stdout);
            drop(stderr);
            drop(dir);
        }
    }

    fn kill_spawned(&self, deadline: Instant) -> Option<u32>
    {
        let mut slot = self.spawned_child();
        let child = slot.as_mut()?;
        let pid = child.id();
        let killed = matches!(child.try_wait(), Ok(None)) && child.kill().is_ok();
        drop(slot);

        // Keep try_wait available to the guard while cleanup polls for exit.
        while matches!(self.spawned_child().as_mut().map(Child::try_wait), Some(Ok(None))) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                eprintln!("ferros-tests: timed out reaping spawned child {pid}");
                break;
            }
            thread::sleep(Duration::from_millis(5).min(remaining));
        }
        killed.then_some(pid)
    }

    fn clean_process(&self, deadline: Instant) -> Vec<u32>
    {
        let mut killed = Vec::new();
        if let ProcessCleanup::Spawned(_) = &self.process {
            killed.extend(self.kill_spawned(deadline));
        }
        let tagged = signal_tagged(&self.tag);
        // Traced children are collected by the backend's exit watcher. Detach transfers
        // the Unix parent's claim to this handle, so collect it before checking the table.
        if let ProcessCleanup::Launched(child) = &self.process
            && let Err(err) = child.reap()
        {
            eprintln!("ferros-tests: detached child cleanup failed: {err}");
        }
        wait_for_killed(&self.tag, &tagged, deadline);
        killed.extend(tagged);
        killed
    }
}

/// State shared between a test's worker thread, its guards and the controlling thread.
#[derive(Default)]
struct Shared
{
    tracked: Mutex<Vec<Arc<Tracked>>>,
    /// Scratch paths removed once processes are cleaned up, on every exit path.
    scratch: Mutex<Vec<PathBuf>>,
    /// Set when the controller gave up on the test; later launches are refused.
    cancelled: AtomicBool,
}

impl Shared
{
    fn tracked(&self) -> MutexGuard<'_, Vec<Arc<Tracked>>>
    {
        self.tracked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn track(&self, tag: String, process: ProcessCleanup, dir: Option<CaptureDir>) -> Arc<Tracked>
    {
        let entry = Arc::new(Tracked {
            tag,
            process,
            capture: Mutex::new(dir.map(|dir| Capture {
                dir,
                stdout: None,
                stderr: None,
            })),
        });
        let mut tracked = self.tracked();
        assert!(
            !self.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to track a process after this test failed its deadline"
        );
        tracked.push(Arc::clone(&entry));
        entry
    }

    /// Registers `path` for removal; removes it at once if the test already failed its
    /// deadline, since the final removal pass may have run.
    fn register_scratch(&self, path: PathBuf)
    {
        let mut scratch = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        if self.cancelled.load(Ordering::SeqCst) {
            drop(scratch);
            remove_scratch_path(&path);
        } else {
            scratch.push(path);
        }
    }

    /// Removes every registered scratch path. Runs after processes are killed so nothing
    /// still holds or recreates them.
    fn remove_scratch(&self)
    {
        let paths = std::mem::take(&mut *self.scratch.lock().unwrap_or_else(PoisonError::into_inner));
        for path in paths {
            remove_scratch_path(&path);
        }
    }

    fn forget(&self, tag: &str)
    {
        self.tracked().retain(|entry| entry.tag != tag);
    }

    fn finish(&self, entry: &Tracked)
    {
        entry.clean_process(Instant::now() + KILL_SETTLE);
        entry.close_capture();
        self.forget(&entry.tag);
    }

    /// Stop capture synchronously. Give all process cleanups one shared settle window
    /// so an unreapable process cannot delay cleanup of another entry.
    fn kill_all(&self) -> Vec<u32>
    {
        let entries = std::mem::take(&mut *self.tracked());
        let deadline = Instant::now() + KILL_SETTLE;
        let (done, results) = mpsc::channel();
        for entry in &entries {
            let entry = Arc::clone(entry);
            let done = done.clone();
            if let Err(err) = thread::Builder::new()
                .name("ferros-process-cleanup".to_owned())
                .spawn(move || {
                    let _ = done.send(entry.clean_process(deadline));
                })
            {
                eprintln!("ferros-tests: could not start process cleanup: {err}");
            }
        }
        drop(done);
        for entry in entries {
            entry.close_capture();
        }
        let mut killed = Vec::new();
        loop {
            match results.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(pids) => killed.extend(pids),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("ferros-tests: process cleanup did not finish within {KILL_SETTLE:?}");
                    break;
                }
            }
        }
        killed.sort_unstable();
        killed.dedup();
        killed
    }
}

/// Removes a file or directory tree, ignoring a path that is already gone.
fn remove_scratch_path(path: &Path)
{
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => return,
    };
    if let Err(err) = result {
        eprintln!("ferros-tests: could not remove scratch path {}: {err}", path.display());
    }
}

/// Handle passed to a test body by [`run`]; launches fixtures and tracks them for cleanup.
pub struct TestContext
{
    shared: Arc<Shared>,
    events: Sender<Event>,
}

impl TestContext
{
    /// Launches `fixture` with no extra arguments. See [`TestContext::launch_with_args`].
    ///
    /// # Panics
    ///
    /// Panics when the launch fails; see [`TestContext::launch_with_args`].
    #[must_use]
    pub fn launch(&self, fixture: &Fixture) -> Debuggee
    {
        self.launch_with_args(fixture, &[])
    }

    /// Launches `fixture` under a new [`Session`], suspended at entry, with `args` as its
    /// arguments (followed by the harness's identifying argument).
    ///
    /// The debuggee's stdout and stderr go to two files in a temporary directory the
    /// harness creates for this launch (they never reach the test's output); their contents
    /// are available through [`Debuggee::stdout`] and [`Debuggee::stderr`]. Its stdin is
    /// the null device.
    ///
    /// # Panics
    ///
    /// Panics when the launch fails. A denied `task_for_pid`, or a launch that does not return
    /// within [`LAUNCH_DEADLINE`], fails with guidance on signing the test executable. Any
    /// process left behind by the failed launch is killed first.
    #[must_use]
    pub fn launch_with_args(&self, fixture: &Fixture, args: &[&str]) -> Debuggee
    {
        match self.try_launch_with_args(fixture, args) {
            Ok(debuggee) => debuggee,
            Err(DebuggerError::PermissionDenied(detail)) => panic!("{}", permission_guidance(&detail)),
            Err(err) => panic!("ferros-tests: failed to launch fixture '{}': {err}", fixture.name()),
        }
    }

    /// Like [`TestContext::launch_with_args`], but returns launch errors instead of panicking.
    ///
    /// Whatever the result, no process from a failed launch survives this call.
    ///
    /// # Errors
    ///
    /// Returns the error from `MacOSBackend::new` or `Session::launch`, or an I/O error
    /// when the capture directory cannot be created.
    ///
    /// # Panics
    ///
    /// Panics when the test has already been failed by its deadline, when the fixture path
    /// is not valid UTF-8, or when the launch deadline passes (in that case the controlling
    /// thread fails the test with the permission guidance).
    pub fn try_launch_with_args(&self, fixture: &Fixture, args: &[&str]) -> Result<Debuggee, DebuggerError>
    {
        self.try_launch_with_backend(fixture, args, ferros_macos::MacOSBackend::new()?)
    }

    /// Like [`TestContext::launch_with_args`], but the session drives `backend`, which the
    /// test may have prepared first (for example to keep a test hook handle of it).
    ///
    /// # Panics
    ///
    /// As for [`TestContext::launch_with_args`].
    #[must_use]
    pub fn launch_with_backend(&self, fixture: &Fixture, args: &[&str], backend: ferros_macos::MacOSBackend) -> Debuggee
    {
        match self.try_launch_with_backend(fixture, args, backend) {
            Ok(debuggee) => debuggee,
            Err(DebuggerError::PermissionDenied(detail)) => panic!("{}", permission_guidance(&detail)),
            Err(err) => panic!("ferros-tests: failed to launch fixture '{}': {err}", fixture.name()),
        }
    }

    fn try_launch_with_backend(
        &self,
        fixture: &Fixture,
        args: &[&str],
        backend: ferros_macos::MacOSBackend,
    ) -> Result<Debuggee, DebuggerError>
    {
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to launch '{}': this test already failed its deadline",
            fixture.name()
        );

        let tag = unique_tag();
        let capture = CaptureDir::create(&tag)?;
        let spec = LaunchSpec::new(fixture.path())
            .args(args.iter().copied())
            .arg(tag.as_str())
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Path(capture.stdout_path()))
            .stderr(StdioSpec::Path(capture.stderr_path()));

        let child_cleanup = backend.child_cleanup();
        let mut session = ferros_macos::session_with(backend);

        let entry = self
            .shared
            .track(tag.clone(), ProcessCleanup::Launched(child_cleanup), Some(capture));

        // Watchdog: if launch has not returned by the deadline, it tells the controller (which
        // fails the test) and kills the half-launched process by its tag.
        let events = self.events.clone();
        let watchdog = LaunchWatchdog::arm(tag, LAUNCH_DEADLINE, move || {
            let _ = events.send(Event::LaunchStalled);
        });
        let launched = session.launch(&spec);
        if watchdog.disarm() {
            // The launch outlived its deadline and the test is being failed for it. Whatever
            // launch returned in the end, leave no process and report the same reason.
            signal_tagged(&entry.tag);
            drop(session);
            self.shared.finish(&entry);
            panic!("{}", launch_stalled_guidance());
        }

        let pid = match launched {
            Ok(pid) => pid,
            Err(err) => {
                // A failed launch can leave a suspended child behind (task_for_pid denied
                // after posix_spawn). Kill it by its argument before releasing the session.
                signal_tagged(&entry.tag);
                drop(session);
                self.shared.finish(&entry);
                return Err(err);
            }
        };

        let Some([stdout, stderr]) = entry.start_readers() else {
            // The controller closed the capture while launch was in flight.
            drop(session);
            self.shared.finish(&entry);
            panic!(
                "ferros-tests: launch of '{}' returned after this test already failed its deadline",
                fixture.name()
            );
        };
        let guard = Debuggee {
            session,
            pid,
            stdout,
            stderr,
            entry,
            shared: Arc::clone(&self.shared),
        };

        // The controller may have given up while launch was blocked; do not keep running.
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: launch of '{}' returned after this test already failed its deadline",
            fixture.name()
        );
        Ok(guard)
    }

    /// Starts `fixture` with `args` as an ordinary child process, not under the debugger
    /// (for example to attach to it later). The harness's identifying argument is
    /// appended, stdio is the null device, and the process is killed on the test's
    /// deadline like a launched one.
    ///
    /// # Panics
    ///
    /// Panics when the test has already failed its deadline or the process cannot be
    /// started.
    #[must_use]
    pub fn spawn(&self, fixture: &Fixture, args: &[&str]) -> Spawned
    {
        let entry = self
            .shared
            .track(unique_tag(), ProcessCleanup::Spawned(Mutex::default()), None);
        // Publish the handle under the same lock the controller uses to kill and reap.
        let mut slot = entry.spawned_child();
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to spawn '{}': this test already failed its deadline",
            fixture.name()
        );
        let child = Command::new(fixture.path())
            .args(args)
            .arg(&entry.tag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(err) => {
                self.shared.forget(&entry.tag);
                panic!("ferros-tests: failed to spawn fixture '{}': {err}", fixture.name());
            }
        };
        let pid = ProcessId::from(child.id());
        *slot = Some(child);
        drop(slot);
        let guard = Spawned {
            pid,
            entry,
            shared: Arc::clone(&self.shared),
        };
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: spawn of '{}' returned after this test already failed its deadline",
            fixture.name()
        );
        guard
    }

    /// Registers a file or directory for removal when the test ends: on normal completion,
    /// on a panic in the body, and when the deadline fires (a deadline cannot unwind the
    /// abandoned worker, so a drop guard alone would leak it). Removal runs after the test's
    /// processes are killed and ignores a path that is already gone. Registering after the
    /// deadline fired removes the path immediately.
    pub fn remove_on_cleanup(&self, path: impl Into<PathBuf>)
    {
        self.shared.register_scratch(path.into());
    }

    /// Creates a unique empty file with [`scratch::create_file`] and registers it with
    /// [`TestContext::remove_on_cleanup`].
    ///
    /// # Panics
    ///
    /// Panics when the file cannot be created.
    #[must_use]
    pub fn scratch_file(&self, stem: &str, suffix: &str) -> (File, PathBuf)
    {
        let (file, path) = scratch::create_file(stem, suffix);
        self.remove_on_cleanup(path.clone());
        (file, path)
    }

    /// Reserves a fresh identifying argument (see [`TEST_ID_ARG_PREFIX`]) for processes the
    /// test starts some other way, for example through the `ferros` binary. Every process
    /// whose command line contains it is killed on the test's deadline, like a launched
    /// fixture, and when the returned [`Tag`] is dropped.
    ///
    /// # Panics
    ///
    /// Panics when the test has already failed its deadline.
    #[must_use]
    pub fn tag(&self) -> Tag
    {
        Tag {
            entry: self.shared.track(unique_tag(), ProcessCleanup::Tagged, None),
            shared: Arc::clone(&self.shared),
        }
    }
}

/// An identifying argument reserved by [`TestContext::tag`]. Dropping it kills every
/// process still carrying it.
pub struct Tag
{
    entry: Arc<Tracked>,
    shared: Arc<Shared>,
}

impl Tag
{
    /// The argument, `--ferros-test-id=...`, to pass to the processes it should cover.
    #[must_use]
    pub fn as_str(&self) -> &str
    {
        &self.entry.tag
    }
}

impl Drop for Tag
{
    fn drop(&mut self)
    {
        self.shared.finish(&self.entry);
    }
}

/// A fixture started by [`TestContext::spawn`], not under the debugger. Dropping it kills
/// and reaps the process.
pub struct Spawned
{
    pid: ProcessId,
    entry: Arc<Tracked>,
    shared: Arc<Shared>,
}

impl Spawned
{
    /// Process id of the fixture.
    #[must_use]
    pub fn pid(&self) -> ProcessId
    {
        self.pid
    }

    /// Check whether this fixture exited, collecting its status without blocking.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if querying the child status fails.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>
    {
        self.entry
            .spawned_child()
            .as_mut()
            .ok_or_else(|| std::io::Error::other("spawned child was not registered"))?
            .try_wait()
    }
}

impl Drop for Spawned
{
    fn drop(&mut self)
    {
        self.shared.finish(&self.entry);
    }
}

/// State of one [`LaunchWatchdog`], decided under a lock so that "the launch returned" and
/// "the launch stalled" can never both win.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchState
{
    Armed,
    Disarmed,
    Stalled,
}

/// Watches a single `Session::launch` call.
///
/// If the call has not returned when `deadline` passes, the watchdog declares the launch
/// stalled, calls `on_stall`, and kills every process carrying `tag`. It does the killing
/// itself, and repeats it until the launch call returns, because nobody else may be left to
/// do it: the test's controller may already have finished its own cleanup (its deadline can
/// pass while a launch is in flight, before the child process exists).
struct LaunchWatchdog
{
    state: Arc<Mutex<WatchState>>,
    launch_returned: Sender<()>,
}

impl LaunchWatchdog
{
    fn arm<F>(tag: String, deadline: Duration, on_stall: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        let state = Arc::new(Mutex::new(WatchState::Armed));
        let (launch_returned, wait) = mpsc::channel::<()>();
        let shared_state = Arc::clone(&state);
        let _ = thread::Builder::new()
            .name("ferros-launch-watchdog".to_owned())
            .spawn(move || {
                if wait.recv_timeout(deadline) != Err(RecvTimeoutError::Timeout) {
                    return;
                }
                {
                    let mut state = shared_state.lock().unwrap_or_else(PoisonError::into_inner);
                    if *state != WatchState::Armed {
                        return;
                    }
                    *state = WatchState::Stalled;
                }
                on_stall();
                let give_up = Instant::now() + STALL_SWEEP_LIMIT;
                loop {
                    kill_debuggee(&tag);
                    let launch_still_blocked = wait.recv_timeout(STALL_SWEEP_INTERVAL) == Err(RecvTimeoutError::Timeout);
                    if !launch_still_blocked || Instant::now() >= give_up {
                        break;
                    }
                }
            });
        Self { state, launch_returned }
    }

    /// Call when the launch has returned. Returns `true` when the watchdog had already
    /// declared the launch stalled.
    fn disarm(self) -> bool
    {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let stalled = *state == WatchState::Stalled;
        if !stalled {
            *state = WatchState::Disarmed;
        }
        drop(state);
        // Dropping `launch_returned` wakes the watchdog thread and ends its sweep.
        drop(self.launch_returned);
        stalled
    }
}

/// A launched fixture process together with the [`Session`] debugging it.
///
/// Dereferences to [`Session`], so session methods can be called directly
/// (`debuggee.read_registers()`). Dropping it drops the session first, letting its ownership
/// policy terminate and reap a launched target. A tag sweep catches a detached target;
/// no stale pid is ever signalled. Output readers and capture files are then removed.
/// Cleanup logs any process that remains and never panics.
pub struct Debuggee
{
    session: Session,
    pid: ProcessId,
    stdout: OutputBuffer,
    stderr: OutputBuffer,
    entry: Arc<Tracked>,
    shared: Arc<Shared>,
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
struct CaptureDir
{
    path: PathBuf,
}

impl CaptureDir
{
    /// Creates a fresh directory under the system temporary directory, named after `tag`,
    /// with two empty files for stdout and stderr.
    fn create(tag: &str) -> Result<Self, DebuggerError>
    {
        let name = tag.trim_start_matches("--").replace('=', "-");
        let path = std::env::temp_dir().join(format!("ferros-tests-{name}"));
        fs::create_dir_all(&path)?;
        let dir = Self { path };
        File::create(dir.stdout_path())?;
        File::create(dir.stderr_path())?;
        Ok(dir)
    }

    fn stdout_path(&self) -> PathBuf
    {
        self.path.join("stdout")
    }

    fn stderr_path(&self) -> PathBuf
    {
        self.path.join("stderr")
    }

    fn remove(&self)
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

struct Capture
{
    dir: CaptureDir,
    stdout: Option<OutputReader>,
    stderr: Option<OutputReader>,
}

/// How often an output reader looks for new data after reaching the end of its file.
const TAIL_INTERVAL: Duration = Duration::from_millis(10);

/// Captured output of a debuggee, filled by a background thread that tails a file.
struct OutputBuffer
{
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl OutputBuffer
{
    /// Starts a thread that follows `path` until its reader guard is dropped.
    fn spawn_reader(path: PathBuf) -> (Self, OutputReader)
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

    fn snapshot(&self) -> String
    {
        String::from_utf8_lossy(&self.bytes.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

/// Control of a reader, kept in the shared capture so the controller can join it.
struct OutputReader
{
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
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

/// Returns the pids of live processes whose argument list contains `tag`.
///
/// The process list comes from `ps`, whose own arguments do not contain the tag. `pgrep -f`
/// must not be used here: a `pgrep` carries the tag in its own argument list, so two searches
/// running at the same time (a launch watchdog sweeping while the test thread checks) list
/// each other and report a pid for a process that is not a fixture.
///
/// Exited processes that have not been reaped (zombies) have no argument list and are not
/// returned.
#[must_use]
pub fn tagged_pids(tag: &str) -> Vec<u32>
{
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    pids_with_tag(&String::from_utf8_lossy(&output.stdout), tag)
}

/// Picks the pids out of `ps -o pid=,command=` output whose command line contains `tag`.
fn pids_with_tag(ps_output: &str, tag: &str) -> Vec<u32>
{
    ps_output
        .lines()
        .filter_map(|line| {
            let (pid, command) = line.trim_start().split_once(char::is_whitespace)?;
            if command.contains(tag) { pid.parse().ok() } else { None }
        })
        .collect()
}

/// Kills every live process whose argument list contains `tag`, then waits briefly for them
/// to go away. Returns the pids that were signalled. Never panics.
#[allow(
    clippy::must_use_candidate,
    reason = "called for its side effect; the pid list is informational"
)]
pub fn kill_tagged(tag: &str) -> Vec<u32>
{
    kill_debuggee(tag)
}

/// Returns `true` only when `pid` has no process-table entry. A zombie is still present
/// and is a cleanup failure: launched targets are now reaped by the debugger. Never use
/// a remembered pid for signalling after a session may have reaped it.
#[must_use]
pub fn process_is_gone(pid: u32) -> bool
{
    let Ok(output) = Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    state.is_empty()
}

/// Waits up to `timeout` for [`process_is_gone`] to report `true`.
#[must_use]
pub fn wait_until_gone(pid: u32, timeout: Duration) -> bool
{
    let deadline = Instant::now() + timeout;
    loop {
        if process_is_gone(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Sends SIGKILL only to processes currently carrying `tag`, then waits up to
/// [`KILL_SETTLE`] for them to disappear. Returns the pids signalled.
fn kill_debuggee(tag: &str) -> Vec<u32>
{
    let targets = signal_tagged(tag);
    wait_for_killed(tag, &targets, Instant::now() + KILL_SETTLE);
    targets
}

fn signal_tagged(tag: &str) -> Vec<u32>
{
    let targets: Vec<u32> = tagged_pids(tag).into_iter().filter(|&pid| pid > 1).collect();
    if targets.is_empty() {
        return targets;
    }

    let _ = Command::new("/bin/kill")
        .arg("-9")
        .args(targets.iter().map(ToString::to_string))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    targets
}

fn wait_for_killed(tag: &str, targets: &[u32], deadline: Instant)
{
    if targets.is_empty() {
        return;
    }
    while Instant::now() < deadline {
        if targets.iter().all(|&pid| process_is_gone(pid)) && tagged_pids(tag).is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn unique_tag() -> String
{
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tag = format!("{TEST_ID_ARG_PREFIX}{}-{counter}", std::process::id());
    let _ = write!(tag, "-{nanos}");
    tag
}

/// Builds the failure message for a denied (or stalled) `task_for_pid`.
///
/// `detail` is the engine's error text; only its first line is kept, because the rest advises
/// `sudo`, which tests must not use.
fn permission_guidance(detail: &str) -> String
{
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("<test executable>"));
    let signing_step = format!(
        "Signing step: run the integration tests with `just test-integration`. On arm64 macOS the cargo runner \
         (scripts/cargo-test-runner.sh, configured in .cargo/config.toml) signs the test executable before running it; \
         make sure FERROS_TEST_UNSIGNED is not set.\nTo sign it manually, from the repository root:\n  codesign \
         --entitlements crates/ferros-macos/ferros.entitlements --force --sign - {}\n",
        exe.display()
    );
    let authorization_step = "Authorization step: check `DevToolsSecurity -status` (developer mode must be enabled) and \
                              that macOS is not waiting on a \"Developer Tools Access\" dialog. A machine with nobody at \
                              the screen cannot answer that dialog: see the setup step of the integration job in \
                              .github/workflows/rust.yml.\n";

    let mut message = String::from("ferros-tests: cannot debug the fixture process: task_for_pid was denied.\n\n");
    match executable_has_debugger_entitlement() {
        Some(false) => {
            let _ = writeln!(
                message,
                "The test executable lacks the `{DEBUGGER_ENTITLEMENT}` entitlement (it was not signed)."
            );
            message.push_str(&signing_step);
        }
        Some(true) => {
            let _ = writeln!(
                message,
                "The test executable is signed with the `{DEBUGGER_ENTITLEMENT}` entitlement, so signing is not the \
                 problem."
            );
            message.push_str(authorization_step);
        }
        None => {
            let _ = writeln!(
                message,
                "Could not inspect the test executable's entitlements; it needs `{DEBUGGER_ENTITLEMENT}`."
            );
            message.push_str(&signing_step);
            message.push_str(authorization_step);
        }
    }
    let _ = write!(message, "Detail: {}", detail.lines().next().unwrap_or_default());
    message
}

/// Failure message for a `Session::launch` call that outlived [`LAUNCH_DEADLINE`].
fn launch_stalled_guidance() -> String
{
    permission_guidance(&format!(
        "Session::launch did not return within {LAUNCH_DEADLINE:?}; the system is probably waiting on an authorization \
         dialog for task_for_pid"
    ))
}

/// Checks (once per test binary) whether the running test executable carries the debugger
/// entitlement. Returns `None` when `codesign` cannot be run.
///
/// This only shapes the failure message; it never blocks a launch, so a root run or an
/// unexpected `codesign` output format cannot break a working setup.
fn executable_has_debugger_entitlement() -> Option<bool>
{
    static RESULT: OnceLock<Option<bool>> = OnceLock::new();
    *RESULT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let output = Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-"])
            .arg(exe)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&output.stdout).contains(DEBUGGER_ENTITLEMENT))
    })
}

#[cfg(test)]
mod tests
{
    use std::mem::DropGuard;
    use std::process::Child;

    use super::*;

    /// A long-running stand-in for a half-launched fixture: `yes` repeats its argument forever.
    fn spawn_tagged(tag: &str) -> DropGuard<Child, impl FnOnce(Child) + use<>>
    {
        let child = Command::new("/usr/bin/yes")
            .arg(tag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /usr/bin/yes");
        DropGuard::new(child, |mut child| {
            let _ = child.kill();
            let _ = child.wait();
        })
    }

    #[test]
    fn deadline_joins_capture_readers()
    {
        let tag = unique_tag();
        let dir = CaptureDir::create(&tag).expect("create capture");
        let path = dir.path.clone();
        let (buffers_tx, buffers_rx) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let result = panic::catch_unwind(|| {
            run_with_timeout(Duration::from_secs(2), move |t| {
                let entry = t.shared.track(tag, ProcessCleanup::Tagged, Some(dir));
                let buffers = entry.start_readers().expect("start readers");
                for buffer in &buffers {
                    assert_eq!(Arc::strong_count(&buffer.bytes), 2, "reader must be live before timeout");
                }
                buffers_tx.send(buffers).expect("send buffers");
                let _ = released.recv();
                drop(entry);
            });
        });

        assert!(result.is_err(), "a body that outlives its deadline must fail");
        for buffer in buffers_rx.try_recv().expect("receive buffers") {
            assert_eq!(Arc::strong_count(&buffer.bytes), 1, "reader kept polling after timeout");
        }
        assert!(!path.exists(), "the controller must remove the capture directory");
        drop(release);
    }

    #[test]
    fn deadline_uses_one_settle_window_for_all_tags()
    {
        let (children_tx, children_rx) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let timeout = Duration::from_secs(2);
        let started = Instant::now();
        let result = panic::catch_unwind(|| {
            run_with_timeout(timeout, move |t| {
                let tags = [t.tag(), t.tag(), t.tag()];
                // Keep the Child handles outside the harness. Each killed process will
                // remain a zombie for the entire settle window unless this test reaps it.
                let children = tags.each_ref().map(|tag| spawn_tagged(tag.as_str()));
                children_tx.send(children).expect("send children");
                let _ = released.recv();
                drop(tags);
            });
        });
        let elapsed = started.elapsed();

        assert!(result.is_err(), "a body that outlives its deadline must fail");
        for mut child in children_rx.try_recv().expect("receive children") {
            let status = child.try_wait().expect("try_wait").expect("tagged child was not killed");
            assert!(!status.success(), "tagged child should have been killed");
        }
        drop(release);
        assert!(
            elapsed < timeout + KILL_SETTLE + Duration::from_secs(2),
            "timeout cleanup took {elapsed:?} including the {timeout:?} deadline"
        );
    }

    #[test]
    fn stalled_launch_watchdog_reports_and_kills_the_tagged_process()
    {
        let tag = unique_tag();
        let mut child = spawn_tagged(&tag);
        let (stalled_tx, stalled_rx) = mpsc::channel();

        let watchdog = LaunchWatchdog::arm(tag.clone(), Duration::from_millis(200), move || {
            let _ = stalled_tx.send(());
        });

        stalled_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the watchdog never reported the stall");
        // No controller is involved here: the watchdog itself must end the process.
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(Instant::now() < deadline, "the watchdog did not kill the tagged process");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(!status.success(), "the tagged process should have been killed, got {status}");
        assert!(watchdog.disarm(), "disarm() must report that the launch stalled");
        assert_eq!(tagged_pids(&tag), Vec::<u32>::new());
    }

    #[test]
    fn concurrent_tag_searches_do_not_list_each_other()
    {
        // Nothing carries this tag, so every search must come back empty even while other
        // threads search for the same tag (the watchdog does exactly that during a sweep).
        let tag = unique_tag();
        let searchers: Vec<_> = (0..4)
            .map(|_| {
                let tag = tag.clone();
                thread::spawn(move || (0..10).flat_map(|_| tagged_pids(&tag)).collect::<Vec<u32>>())
            })
            .collect();
        for searcher in searchers {
            assert_eq!(searcher.join().expect("searcher thread"), Vec::<u32>::new());
        }
    }

    #[test]
    fn pids_with_tag_matches_only_command_lines_that_contain_the_tag()
    {
        let ps_output = "    1 /sbin/launchd\n  412 /tmp/ferros-fixture-hello --ferros-test-id=7-0-1\n 5021 /usr/bin/yes \
                         --ferros-test-id=7-1-2\n  733 (yes)\n";
        assert_eq!(pids_with_tag(ps_output, "--ferros-test-id=7-0-1"), vec![412]);
        assert_eq!(pids_with_tag(ps_output, "--ferros-test-id=7-"), vec![412, 5021]);
        assert_eq!(pids_with_tag(ps_output, "--ferros-test-id=9"), Vec::<u32>::new());
    }

    #[test]
    fn disarmed_watchdog_stays_quiet()
    {
        let tag = unique_tag();
        let mut child = spawn_tagged(&tag);
        let (stalled_tx, stalled_rx) = mpsc::channel();

        let watchdog = LaunchWatchdog::arm(tag.clone(), Duration::from_millis(300), move || {
            let _ = stalled_tx.send(());
        });
        assert!(!watchdog.disarm(), "disarm() before the deadline must not report a stall");

        thread::sleep(Duration::from_millis(600));
        assert!(stalled_rx.try_recv().is_err(), "a disarmed watchdog reported a stall");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "a disarmed watchdog killed the process"
        );
    }

    #[test]
    fn permission_guidance_keeps_only_the_first_detail_line()
    {
        let message = permission_guidance("task_for_pid() failed with KERN_FAILURE\n\nQuick fix: Run with sudo:\n  sudo x");
        assert!(message.contains("Detail: task_for_pid() failed with KERN_FAILURE"));
        assert!(!message.contains("sudo x"), "engine sudo advice leaked into: {message}");
        assert!(message.contains(DEBUGGER_ENTITLEMENT));
    }
}
