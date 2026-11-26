//! Lifecycle tests: `holder`.

use std::os::unix::process::ExitStatusExt as _;
use std::process::Command;
use std::time::{Duration, Instant};

use ferros_core::arch::Arm64;
use ferros_core::{BreakpointLocation, ProcessId, Session};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness};

use super::{SIGKILL, TIMEOUT, assert_reaped, process_state};

/// Log lines written by a thread-local subscriber, for checking a backend's `tracing` output.
#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedLog
{
    /// Capture events emitted on the current thread until the guard is dropped.
    fn install(&self) -> impl Sized
    {
        use tracing_subscriber::util::SubscriberInitExt as _;
        let log = self.clone();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || log.clone())
            .finish()
            .set_default()
    }

    fn text(&self) -> String
    {
        String::from_utf8_lossy(&self.0.lock().expect("log lock")).into_owned()
    }
}

impl std::io::Write for CapturedLog
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>
    {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()>
    {
        Ok(())
    }
}

/// Attach to a running `loop_counter`, plant a trap it does not reach, resume it, and
/// make the next interrupt deliver no stop. Returns the session and the trap address.
fn attach_with_an_unreached_trap(backend: MacOSBackend, pid: ProcessId) -> (Session, ferros_core::Address)
{
    let fixture = fixture!("loop_counter");
    let drop_interrupt = backend.interrupt_fault_injector();
    let mut session = ferros_macos::session_with(backend);
    session.attach(pid).expect("attach");
    let id = session
        .set_breakpoint(BreakpointLocation::Line {
            file: format!("{}.rs", fixture.name()),
            line: fixture.marker_line("loop_final"),
        })
        .expect("set breakpoint");
    let site = session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .and_then(|breakpoint| breakpoint.sites.first())
        .expect("resolved breakpoint site")
        .address;
    session.resume().expect("resume");
    drop_interrupt();
    (session, site)
}

fn task_suspend_count(pid: ProcessId) -> i32
{
    diagnostics::process_suspend_counts(pid)
        .expect("target should still be alive")
        .task
        .expect("task suspend count")
}

/// A drop whose interrupt never stops the target holds it with a fresh task suspension:
/// the `suspends == 0` branch of `MacOSBackend::hold_attached_on_drop` (#198).
#[test]
fn attached_session_drop_suspends_the_target_when_no_stop_arrives()
{
    harness::run(|t| {
        // The loop outlives the test, so the trap after it is never reached.
        let mut target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let (session, site) = attach_with_an_unreached_trap(MacOSBackend::new().expect("backend"), pid);
        let ports = diagnostics::exception_ports(pid).expect("debugger exception ports");
        assert_eq!(task_suspend_count(pid), 0, "running target is suspended before drop");

        let log = CapturedLog::default();
        let guard = log.install();
        drop(session);
        drop(guard);
        let log = log.text();
        assert!(
            log.contains("keeping attached target stopped after waiting for a stop timed out"),
            "drop did not time out waiting for the stop:\n{log}"
        );
        assert!(!log.contains("could not suspend"), "hold failed to suspend:\n{log}");

        assert_eq!(
            target.try_wait().expect("check target status"),
            None,
            "target exited after drop"
        );
        assert!(task_suspend_count(pid) > 0, "drop left the target running");
        let state = process_state(pid.0);
        assert!(state.contains('X'), "target is no longer traced: {state}");
        assert_eq!(diagnostics::exception_ports(pid).expect("ports after drop"), ports);
        assert_eq!(
            diagnostics::raw_memory(pid, site, Arm64::TRAP_INSTRUCTION.len()).expect("read retained instruction"),
            Arm64::TRAP_INSTRUCTION
        );
        drop(target); // Harness kills and reaps the retained fixture.
        assert_reaped(pid.0);
    });
}

/// When the hold cannot suspend the target either, the failure is logged and the target
/// stays traced, with the debugger's exception ports, rather than being detached with
/// its trap still planted (#198).
#[test]
fn attached_session_drop_logs_a_hold_that_cannot_suspend()
{
    harness::run(|t| {
        let mut target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let backend = MacOSBackend::new().expect("backend");
        let fail_suspend = backend.hold_suspend_fault_injector();
        let (session, site) = attach_with_an_unreached_trap(backend, pid);
        let ports = diagnostics::exception_ports(pid).expect("debugger exception ports");

        fail_suspend();
        let log = CapturedLog::default();
        let guard = log.install();
        drop(session);
        drop(guard);
        let log = log.text();
        assert!(
            log.contains(&format!("could not suspend retained target {}", pid.0)),
            "failed hold suspension was not logged:\n{log}"
        );

        assert_eq!(
            target.try_wait().expect("check target status"),
            None,
            "target exited after drop"
        );
        assert_eq!(task_suspend_count(pid), 0, "no suspension was injected away");
        let state = process_state(pid.0);
        assert!(state.contains('X'), "target is no longer traced: {state}");
        assert_eq!(diagnostics::exception_ports(pid).expect("ports after drop"), ports);
        assert_eq!(
            diagnostics::raw_memory(pid, site, Arm64::TRAP_INSTRUCTION.len()).expect("read retained instruction"),
            Arm64::TRAP_INSTRUCTION
        );
        drop(target); // Harness kills and reaps the retained fixture.
        assert_reaped(pid.0);
    });
}

/// Set in the re-executed test binary that holds a target and then exits.
const HOLDER_TARGET_ENV: &str = "FERROS_TEST_HOLDER_TARGET";

/// What happens to a held target when the debugger process that holds it exits (#198).
///
/// The test binary re-executes itself (it carries the debugger entitlement) as a
/// separate debugger: it attaches, holds the target on drop, and exits with
/// `std::process::exit`, as the CLI does after dropping its session.
#[test]
fn held_target_when_the_debugger_process_exits()
{
    if let Ok(pid) = std::env::var(HOLDER_TARGET_ENV) {
        hold_and_exit(ProcessId(pid.parse().expect("holder target pid")));
    }
    harness::run(|t| {
        let mut target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let tag = t.tag();
        let holder = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "--nocapture", "--"])
            // libtest names it by module path inside the `native` binary.
            .arg("lifecycle::holder::held_target_when_the_debugger_process_exits")
            .arg(tag.as_str())
            .env(HOLDER_TARGET_ENV, pid.0.to_string())
            .output()
            .expect("run holder");
        let stdout = String::from_utf8_lossy(&holder.stdout);
        let stderr = String::from_utf8_lossy(&holder.stderr);
        assert!(
            holder.status.success() && stdout.contains("holder: held"),
            "holder failed: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            holder.status
        );
        let held = stdout
            .lines()
            .find_map(|line| line.strip_prefix("holder: held "))
            .expect("holder report");
        assert!(held.contains('X'), "holder did not leave the target traced: {held}");
        assert!(
            !held.ends_with("task_suspends=0"),
            "holder did not suspend the target: {held}"
        );

        // The holder has exited; XNU's tracer-exit path must now kill its tracee.
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = target.try_wait().expect("check target status") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "held target {} outlived its debugger: state {:?}, task suspends {:?}",
                pid.0,
                process_state(pid.0),
                diagnostics::process_suspend_counts(pid).map(|counts| counts.task)
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(SIGKILL), "held target ended with {status:?}");
        drop(target);
        assert_reaped(pid.0);
    });
}

/// The holder side of [`held_target_when_the_debugger_process_exits`]: never returns.
fn hold_and_exit(pid: ProcessId) -> !
{
    let (session, _) = attach_with_an_unreached_trap(MacOSBackend::new().expect("backend"), pid);
    drop(session);
    let state = process_state(pid.0);
    let suspends = task_suspend_count(pid);
    println!("holder: held state={state} task_suspends={suspends}");
    std::process::exit(0);
}
