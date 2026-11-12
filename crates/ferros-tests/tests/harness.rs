//! Self-tests for the integration harness: cleanup on every path, timeouts, drop order.
#![cfg(target_os = "macos")]

use std::panic::{self, AssertUnwindSafe};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ferros_core::SessionState;
use ferros_tests::fixture;
use ferros_tests::harness::{self, TEST_ID_ARG_PREFIX};

/// Pid and identifying argument of a debuggee, handed out of a test body.
type Seen = Arc<Mutex<Option<(u32, String)>>>;

fn seen_debuggee(seen: &Seen) -> (u32, String)
{
    seen.lock()
        .expect("seen lock")
        .clone()
        .expect("the body never launched its debuggee")
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String
{
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

fn assert_no_process(pid: u32, tag: &str)
{
    assert!(
        harness::wait_until_gone(pid, Duration::from_secs(2)),
        "debuggee {pid} is still running"
    );
    assert!(
        harness::tagged_pids(tag).is_empty(),
        "a process tagged {tag} is still running"
    );
}

#[test]
fn panic_in_body_kills_debuggee()
{
    let seen = Seen::default();
    let slot = Arc::clone(&seen);
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        harness::run(move |t| {
            let d = t.launch_with_args(&fixture!("loop_counter"), &["3000"]);
            *slot.lock().expect("slot lock") = Some((d.pid().0, d.tag().to_owned()));
            panic!("intentional panic from the test body");
        });
    }));

    let payload = result.expect_err("harness::run should propagate the body's panic");
    assert_eq!(panic_message(&*payload), "intentional panic from the test body");
    let (pid, tag) = seen_debuggee(&seen);
    assert_no_process(pid, &tag);
}

#[test]
fn deadline_kills_debuggee_and_reports_timeout()
{
    let seen = Seen::default();
    let slot = Arc::clone(&seen);
    let started = Instant::now();
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        harness::run_with_timeout(Duration::from_secs(2), move |t| {
            let d = t.launch_with_args(&fixture!("loop_counter"), &["3000"]);
            *slot.lock().expect("slot lock") = Some((d.pid().0, d.tag().to_owned()));
            thread::sleep(Duration::from_secs(60));
            drop(d);
        });
    }));
    let elapsed = started.elapsed();

    let payload = result.expect_err("a body that outlives its deadline must fail");
    let message = panic_message(&*payload);
    assert!(message.contains("timed out"), "unexpected timeout message: {message}");
    assert!(elapsed < Duration::from_secs(10), "timeout took {elapsed:?}");
    let (pid, tag) = seen_debuggee(&seen);
    assert_no_process(pid, &tag);
}

#[test]
fn drop_while_suspended_at_entry_kills_debuggee()
{
    harness::run(|t| {
        let d = t.launch_with_args(&fixture!("loop_counter"), &["3000"]);
        assert_eq!(d.state(), SessionState::Stopped);
        let (pid, tag) = (d.pid().0, d.tag().to_owned());
        let started = Instant::now();
        drop(d);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "drop took {:?}",
            started.elapsed()
        );
        assert_no_process(pid, &tag);
    });
}

#[test]
fn drop_while_running_kills_debuggee()
{
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &["3000"]);
        d.resume().expect("resume");
        let (pid, tag) = (d.pid().0, d.tag().to_owned());
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        drop(d);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "drop took {:?}",
            started.elapsed()
        );
        assert_no_process(pid, &tag);
    });
}

#[test]
fn drop_after_process_exited_by_itself()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        d.resume().expect("resume");
        let pid = d.pid().0;
        assert!(d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)));
        assert!(harness::wait_until_gone(pid, Duration::from_secs(10)), "hello did not exit");
        let started = Instant::now();
        drop(d);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "drop took {:?}",
            started.elapsed()
        );
    });
}

#[test]
fn drop_after_detach_kills_debuggee()
{
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &["3000"]);
        d.detach().expect("detach");
        let (pid, tag) = (d.pid().0, d.tag().to_owned());
        // Detaching does not end a launched process; only the guard's kill does.
        thread::sleep(Duration::from_millis(100));
        assert!(!harness::process_is_gone(pid), "detach unexpectedly ended the debuggee");
        drop(d);
        assert_no_process(pid, &tag);
    });
}

/// Ends a directly spawned process even when an assertion fails first.
struct KillOnDrop(Child);

impl Drop for KillOnDrop
{
    fn drop(&mut self)
    {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The orphan left by a failed launch is found and killed by its identifying argument.
#[test]
fn kill_tagged_kills_orphan_by_argument()
{
    let tag = format!("{TEST_ID_ARG_PREFIX}orphan-check-{}", std::process::id());
    let mut child = KillOnDrop(
        Command::new(fixture!("loop_counter").path())
            .args(["3000", &tag])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn loop_counter"),
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while harness::tagged_pids(&tag).is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let killed = harness::kill_tagged(&tag);
    assert_eq!(killed, vec![child.0.id()]);
    let status = child.0.wait().expect("wait for loop_counter");
    assert!(!status.success(), "loop_counter should have been killed");
    assert_eq!(harness::tagged_pids(&tag), Vec::<u32>::new());
}

/// Opt-in failure for a manual leak check: `FERROS_TEST_FORCE_FAIL=1 just test-integration`
/// must fail, and afterwards `pgrep -f ferros-fixture-` must print nothing.
#[test]
fn forced_failure_for_leak_check()
{
    if std::env::var_os("FERROS_TEST_FORCE_FAIL").is_none_or(|value| value != "1") {
        return;
    }
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &["3000"]);
        d.resume().expect("resume");
        panic!(
            "FERROS_TEST_FORCE_FAIL=1: failing on purpose with a running debuggee (pid {})",
            d.pid().0
        );
    });
}
