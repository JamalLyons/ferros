use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::process::kill_debuggee;
use super::{DEBUGGER_ENTITLEMENT, LAUNCH_DEADLINE};

/// After a launch stalled, how often and for how long its watchdog keeps looking for the
/// half-launched process while `Session::launch` has still not returned.
const STALL_SWEEP_INTERVAL: Duration = Duration::from_millis(200);
const STALL_SWEEP_LIMIT: Duration = Duration::from_secs(30);

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
pub(super) struct LaunchWatchdog
{
    state: Arc<Mutex<WatchState>>,
    launch_returned: Sender<()>,
}

impl LaunchWatchdog
{
    pub(super) fn arm<F>(tag: String, deadline: Duration, on_stall: F) -> Self
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
    pub(super) fn disarm(self) -> bool
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

/// Builds the failure message for a denied (or stalled) `task_for_pid`.
pub(super) fn permission_guidance(detail: &str) -> String
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
    let _ = write!(message, "Detail: {detail}");
    message
}

/// Failure message for a `Session::launch` call that outlived [`LAUNCH_DEADLINE`].
pub(super) fn launch_stalled_guidance() -> String
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
