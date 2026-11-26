use std::fmt::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::TEST_ID_ARG_PREFIX;

/// How long cleanup waits for a killed process to disappear before giving up.
pub(super) const KILL_SETTLE: Duration = Duration::from_secs(2);

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
pub(super) fn pids_with_tag(ps_output: &str, tag: &str) -> Vec<u32>
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
pub(super) fn kill_debuggee(tag: &str) -> Vec<u32>
{
    let targets = signal_tagged(tag);
    wait_for_killed(tag, &targets, Instant::now() + KILL_SETTLE);
    targets
}

pub(super) fn signal_tagged(tag: &str) -> Vec<u32>
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

pub(super) fn wait_for_killed(tag: &str, targets: &[u32], deadline: Instant)
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

pub(super) fn unique_tag() -> String
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
