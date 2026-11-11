//! Harness for integration tests that debug real fixture processes (macOS only).
//!
//! Every integration test body runs under [`run`] (or [`run_with_timeout`]), which gives it a
//! [`TestContext`]. The context launches fixtures under a fresh debugger and returns a
//! [`Debuggee`] guard that owns the debugger and kills the debuggee when dropped.
//!
//! ```ignore
//! use ferros_tests::{fixture, harness};
//!
//! harness::run(|t| {
//!     let mut d = t.launch(&fixture!("hello"));
//!     assert!(d.is_stopped());
//!     d.resume().expect("resume");
//!     assert!(d.wait_for_stdout("hello from ferros fixture", std::time::Duration::from_secs(10)));
//! });
//! ```
//!
//! Guarantees:
//!
//! - **No leaked processes.** Each launch appends a unique `--ferros-test-id=...` argument to
//!   the fixture's argv. Dropping the [`Debuggee`] (normal return or unwinding) kills the
//!   process by pid and by that argument; a failed launch kills any process left behind by
//!   the argument; a timeout kills every process the test launched.
//! - **Hard deadline.** The body runs on a worker thread. If it does not finish in time the
//!   test fails with a "timed out" message after its debuggees are killed. Only that test's
//!   processes are touched, so tests can run in parallel.
//! - **Fast permission failures.** If `task_for_pid` is denied, or `Debugger::launch` does not
//!   return within [`LAUNCH_DEADLINE`] (for example because the system is waiting on an
//!   authorization dialog), the test fails with guidance on signing the test executable.
//! - **Panics stay panics.** A panic in the body reaches libtest with its original payload.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferros_core::debugger::create_debugger;
use ferros_core::{Debugger, DebuggerError, ProcessId};

use crate::Fixture;

/// Deadline applied by [`run`] to a whole test body.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`TestContext::launch`] waits for `Debugger::launch` to return before it treats
/// the launch as blocked on a permission prompt and fails the test.
pub const LAUNCH_DEADLINE: Duration = Duration::from_secs(5);

/// Prefix of the identifying argument appended as the last argv entry of every launched
/// fixture. `pgrep -f -- --ferros-test-id=` lists fixtures started by this harness.
pub const TEST_ID_ARG_PREFIX: &str = "--ferros-test-id=";

/// Entitlement the test executable needs to call `task_for_pid` without root.
pub const DEBUGGER_ENTITLEMENT: &str = "com.apple.security.cs.debugger";

/// How long cleanup waits for a killed process to disappear before giving up.
const KILL_SETTLE: Duration = Duration::from_secs(2);

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
/// the worker thread is abandoned (it holds no process any more).
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

    match outcome {
        Outcome::Finished(Ok(())) => {}
        Outcome::Finished(Err(payload)) => panic::resume_unwind(payload),
        Outcome::LaunchStalled => panic!(
            "{}",
            permission_guidance(&format!(
                "Debugger::launch did not return within {LAUNCH_DEADLINE:?}; the system is probably waiting on an \
                 authorization dialog for task_for_pid"
            ))
        ),
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

/// A process launched by one test: its identifying argument and, once known, its pid.
struct Tracked
{
    tag: String,
    pid: Option<u32>,
}

/// State shared between a test's worker thread, its guards and the controlling thread.
#[derive(Default)]
struct Shared
{
    tracked: Mutex<Vec<Tracked>>,
    /// Set when the controller gave up on the test; later launches are refused.
    cancelled: AtomicBool,
}

impl Shared
{
    fn tracked(&self) -> MutexGuard<'_, Vec<Tracked>>
    {
        self.tracked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_pid(&self, tag: &str, pid: u32)
    {
        if let Some(entry) = self.tracked().iter_mut().find(|entry| entry.tag == tag) {
            entry.pid = Some(pid);
        }
    }

    fn forget(&self, tag: &str)
    {
        self.tracked().retain(|entry| entry.tag != tag);
    }

    /// Kills every process still tracked and returns the pids that were targeted.
    fn kill_all(&self) -> Vec<u32>
    {
        let entries = std::mem::take(&mut *self.tracked());
        let mut killed = Vec::new();
        for entry in entries {
            killed.extend(kill_debuggee(entry.pid, &entry.tag));
        }
        killed
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

    /// Launches `fixture` under a new debugger, suspended at entry, with `args` as its
    /// arguments (followed by the harness's identifying argument).
    ///
    /// The debuggee's stdout and stderr are captured (they never reach the test's output) and
    /// are available through [`Debuggee::stdout`] and [`Debuggee::stderr`].
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
    /// Returns the error from `create_debugger` or `Debugger::launch`.
    ///
    /// # Panics
    ///
    /// Panics when the test has already been failed by its deadline, when the fixture path
    /// is not valid UTF-8, or when the launch deadline passes (in that case the controlling
    /// thread fails the test with the permission guidance).
    pub fn try_launch_with_args(&self, fixture: &Fixture, args: &[&str]) -> Result<Debuggee, DebuggerError>
    {
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to launch '{}': this test already failed its deadline",
            fixture.name()
        );

        let program = fixture
            .path()
            .to_str()
            .unwrap_or_else(|| panic!("fixture path is not UTF-8: {}", fixture.path().display()));
        let tag = unique_tag();
        let mut command_line = Vec::with_capacity(args.len() + 2);
        command_line.push(program);
        command_line.extend_from_slice(args);
        command_line.push(&tag);

        let mut debugger = create_debugger()?;
        debugger.set_capture_process_output(true);

        self.shared.tracked().push(Tracked {
            tag: tag.clone(),
            pid: None,
        });

        // Watchdog: if launch has not returned by the deadline, tell the controller, which
        // kills the tagged process and fails the test. Dropping `launch_done` disarms it.
        let (launch_done, launch_wait) = mpsc::channel::<()>();
        let events = self.events.clone();
        let _ = thread::Builder::new()
            .name("ferros-launch-watchdog".to_owned())
            .spawn(move || {
                if launch_wait.recv_timeout(LAUNCH_DEADLINE) == Err(RecvTimeoutError::Timeout) {
                    let _ = events.send(Event::LaunchStalled);
                }
            });
        let launched = debugger.launch(program, &command_line);
        drop(launch_done);

        let pid = match launched {
            Ok(pid) => pid,
            Err(err) => {
                // A failed launch can leave a suspended child behind (task_for_pid denied
                // after posix_spawn). Kill it by its argument before releasing the debugger.
                kill_debuggee(None, &tag);
                self.shared.forget(&tag);
                drop(debugger);
                return Err(err);
            }
        };
        self.shared.set_pid(&tag, pid.0);

        let stdout = OutputBuffer::spawn_reader(debugger.take_process_stdout());
        let stderr = OutputBuffer::spawn_reader(debugger.take_process_stderr());
        let guard = Debuggee {
            debugger,
            pid,
            tag,
            stdout,
            stderr,
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
}

/// A launched fixture process together with the debugger attached to it.
///
/// Dereferences to `dyn Debugger`, so debugger methods can be called directly
/// (`debuggee.read_registers()`). Dropping it kills the process (`kill -9` by pid and by its
/// identifying argument) and then drops the debugger; this is safe whether the process is
/// suspended at entry, running, already exited, or detached. Drop never panics and waits at
/// most about two seconds.
pub struct Debuggee
{
    debugger: Box<dyn Debugger>,
    pid: ProcessId,
    tag: String,
    stdout: OutputBuffer,
    stderr: OutputBuffer,
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
        &self.tag
    }

    /// The debugger attached to this process.
    #[must_use]
    pub fn debugger(&self) -> &dyn Debugger
    {
        &*self.debugger
    }

    /// Mutable access to the debugger attached to this process.
    #[must_use]
    pub fn debugger_mut(&mut self) -> &mut dyn Debugger
    {
        &mut *self.debugger
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
    type Target = dyn Debugger;

    fn deref(&self) -> &Self::Target
    {
        &*self.debugger
    }
}

impl DerefMut for Debuggee
{
    fn deref_mut(&mut self) -> &mut Self::Target
    {
        &mut *self.debugger
    }
}

impl Drop for Debuggee
{
    fn drop(&mut self)
    {
        // Kill before the debugger is dropped (fields drop after this function): neither
        // detach nor dropping the debugger ends a launched process, and a process left
        // suspended at entry would otherwise survive forever.
        kill_debuggee(Some(self.pid.0), &self.tag);
        self.shared.forget(&self.tag);
    }
}

/// Captured output of a debuggee, filled by a background reader thread until EOF.
struct OutputBuffer
{
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl OutputBuffer
{
    fn spawn_reader(file: Option<File>) -> Self
    {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        if let Some(mut file) = file {
            let sink = Arc::clone(&bytes);
            // The reader ends at EOF, which arrives once the debuggee is gone.
            let _ = thread::Builder::new().name("ferros-output-reader".to_owned()).spawn(move || {
                let mut chunk = vec![0_u8; 4096];
                while let Ok(read @ 1..) = file.read(&mut chunk) {
                    sink.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(&chunk[..read]);
                }
            });
        }
        Self { bytes }
    }

    fn snapshot(&self) -> String
    {
        String::from_utf8_lossy(&self.bytes.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

/// Returns the pids of live processes whose argument list contains `tag` (`pgrep -f`).
///
/// Exited processes that have not been reaped (zombies) have no argument list and are not
/// returned.
#[must_use]
pub fn tagged_pids(tag: &str) -> Vec<u32>
{
    let Ok(output) = Command::new("/usr/bin/pgrep")
        .arg("-f")
        .arg("--")
        .arg(tag)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
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
    kill_debuggee(None, tag)
}

/// Returns `true` when `pid` no longer runs: either no such process exists or it exited and
/// only its zombie entry remains (the harness never reaps debuggees, so their pids cannot be
/// reused while the test binary runs).
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
    state.is_empty() || state.starts_with('Z')
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

/// Sends SIGKILL to `pid` (when given) and to every process tagged `tag`, then waits up to
/// [`KILL_SETTLE`] for them to disappear. Returns the pids signalled.
fn kill_debuggee(pid: Option<u32>, tag: &str) -> Vec<u32>
{
    let mut targets: Vec<u32> = pid.into_iter().filter(|&pid| pid > 1).collect();
    for tagged in tagged_pids(tag) {
        if tagged > 1 && !targets.contains(&tagged) {
            targets.push(tagged);
        }
    }
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

    let deadline = Instant::now() + KILL_SETTLE;
    while !(targets.iter().all(|&pid| process_is_gone(pid)) && tagged_pids(tag).is_empty()) {
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    targets
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
fn permission_guidance(detail: &str) -> String
{
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("<test executable>"));
    let mut message = format!(
        "ferros-tests: cannot debug the fixture process: task_for_pid was denied.\n\nThe test executable lacks the \
         `{DEBUGGER_ENTITLEMENT}` entitlement.\nRun the integration tests with `just test-integration`: on arm64 macOS the \
         cargo runner (scripts/cargo-test-runner.sh, configured in .cargo/config.toml) signs the test executable before \
         running it. Make sure FERROS_TEST_UNSIGNED is not set.\nTo sign it manually, from the repository root:\n  \
         codesign --entitlements crates/ferros-core/ferros.entitlements --force --sign - {}\nIf the executable is signed \
         and still denied, check `DevToolsSecurity -status` (developer mode must be enabled).\n",
        exe.display()
    );
    let _ = match executable_has_debugger_entitlement() {
        Some(true) => writeln!(message, "Preflight: the test executable does carry the entitlement."),
        Some(false) => writeln!(message, "Preflight: the test executable does NOT carry the entitlement."),
        None => writeln!(message, "Preflight: could not inspect the test executable's entitlements."),
    };
    let _ = write!(message, "Detail: {detail}");
    message
}

/// Checks (once per test binary) whether the running test executable carries the debugger
/// entitlement. Returns `None` when `codesign` cannot be run.
///
/// This only enriches the failure message; it never blocks a launch, so a root run or an
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
