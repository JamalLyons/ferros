//! Mach exception plumbing of the macOS backend: simultaneous stops are queued, the
//! port rights of exception messages are released, detaching restores the target's
//! exception ports, and a failed exception reply or task suspend is reported without
//! ending stop reporting.
#![cfg(target_os = "macos")]

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ferros_core::{Address, BreakpointRequest, FaultKind, ResumeRequest, SessionState, StopEvent, ThreadAction, ThreadId};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::control::ControlDir;
use ferros_tests::fixture;
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::symbols::marker_address;

/// How long a test waits for a stop after resuming or interrupting.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Iterations passed to `loop_counter`, so it outlives every test (10 ms each).
const LONG_LOOP: &str = "100000";

/// The tests of this file run one at a time. The port count test measures the whole IPC
/// space of this process, which another test launching and attaching at the same time
/// would change.
fn serial() -> MutexGuard<'static, ()>
{
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wait for the next stop and require a breakpoint stop at `address`; returns its thread.
fn expect_breakpoint(d: &mut Debuggee, address: Address, what: &str) -> ThreadId
{
    match d
        .wait(Some(STOP_TIMEOUT))
        .unwrap_or_else(|err| panic!("wait for {what}: {err}"))
    {
        Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
        other => panic!("expected {what} (a breakpoint stop at {address}), got {other:?}"),
    }
}

/// Wait for the stop of an interrupt.
fn expect_interrupted(d: &mut Debuggee, what: &str)
{
    let event = d
        .wait(Some(STOP_TIMEOUT))
        .unwrap_or_else(|err| panic!("wait for {what}: {err}"));
    assert!(
        matches!(event, Some(StopEvent::Interrupted { .. })),
        "expected {what} (an interrupt stop), got {event:?}"
    );
}

/// Ask the running target to stop through the session.
fn interrupt(d: &mut Debuggee)
{
    d.interrupt().expect("interrupt");
}

/// How many launches the simultaneous-stop test tries before giving up.
const SIMULTANEOUS_TRIALS: usize = 20;

/// Two threads of `threads` leave a barrier together and run into the same breakpoint.
/// When both trap before the debugger has stopped the task, both stops are reported with
/// no resume in between: the second by a zero-timeout `wait` after the first.
///
/// Whether the second thread reaches the trap before the task is suspended is a race, so
/// the test launches the fixture up to [`SIMULTANEOUS_TRIALS`] times and passes on the
/// first launch that shows both stops. Without queued stops, the second message stays
/// in the port until the next resume, so no launch ever does.
#[test]
fn simultaneous_breakpoints_on_two_threads_are_both_reported()
{
    let _serial = serial();
    harness::run_with_timeout(Duration::from_secs(120), |t| {
        let fixture = fixture!("threads");
        let mut outcomes = Vec::new();
        for trial in 1..=SIMULTANEOUS_TRIALS {
            let mut d = t.launch(&fixture);
            let address = marker_address(&mut d, &fixture, "worker_body");
            let id = d
                .add_breakpoint(BreakpointRequest::Software { address })
                .expect("add_breakpoint at worker_body");
            d.resume().expect("resume");

            let first = expect_breakpoint(&mut d, address, "the first worker's stop");
            // No resume: anything reported now belongs to the same stop.
            let second = d.wait(Some(Duration::ZERO)).expect("wait(0) after the first stop");
            // Only the simultaneous stop matters here: remove the breakpoint before going on.
            d.remove_breakpoint(id).expect("remove_breakpoint");
            match second {
                Some(StopEvent::Breakpoint { thread, pc }) => {
                    assert_eq!(pc, address, "the second stop is at another pc");
                    assert_ne!(thread, first, "both stops name the same thread");
                    assert_eq!(d.state(), SessionState::Stopped);
                    eprintln!("both workers' stops reported on launch {trial} of {SIMULTANEOUS_TRIALS}");
                    return;
                }
                None => outcomes.push("only one stop"),
                other => panic!("expected the second worker's breakpoint stop or nothing, got {other:?}"),
            }
        }
        panic!("no launch out of {SIMULTANEOUS_TRIALS} reported both workers' stops without a resume: {outcomes:?}");
    });
}

/// Attaching replaces a process's exception ports with the debugger's handler; detaching
/// puts the original ones back.
#[test]
fn detach_restores_the_exception_ports_from_before_attach()
{
    let _serial = serial();
    harness::run(|t| {
        // Started outside the debugger, so this is a real attach.
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let before = diagnostics::exception_ports(pid).expect("exception ports before attach");

        let mut session = ferros_macos::new_session().expect("MacOSBackend::new");
        session.attach(pid).expect("attach");
        let attached = diagnostics::exception_ports(pid).expect("exception ports while attached");
        assert_ne!(
            attached, before,
            "attaching did not install the debugger's exception port: {attached:?}"
        );

        session.detach().expect("detach");
        let after = diagnostics::exception_ports(pid).expect("exception ports after detach");
        assert_eq!(
            after, before,
            "detach left other exception ports than the process had before attach"
        );
        // The detached process keeps running.
        assert!(!harness::process_is_gone(pid.0), "the process died after detach");
    });
}

/// Breakpoint stops before the measurement starts, so lazily created state exists.
const WARMUP_STOPS: usize = 5;
/// Breakpoint stops measured.
const MEASURED_STOPS: usize = 50;
/// Allowed growth of the name count and of the send-right references between the two
/// measurements. Nothing in this cycle should create a port, but the IPC space is the
/// whole test process's: a thread of the harness that ends or starts in between changes
/// it by one. Leaking the two rights of each exception message (thread and task) would
/// add at least `2 * MEASURED_STOPS`.
const PORT_TOLERANCE: u64 = 2;

/// Stopping a fixture many times does not grow the debugger's port names or send-right
/// references: each exception message's thread and task rights are released.
#[test]
fn repeated_stops_do_not_grow_the_debuggers_ports()
{
    let _serial = serial();
    harness::run_with_timeout(Duration::from_secs(120), |t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let address = marker_address(&mut d, &fixture, "tick_body");
        let id = d
            .add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        d.resume().expect("resume");

        let measure = || {
            (
                diagnostics::port_name_count().expect("port_name_count"),
                diagnostics::send_right_references().expect("send_right_references"),
            )
        };
        let mut baseline = None;
        for stop in 1..=WARMUP_STOPS + MEASURED_STOPS {
            expect_breakpoint(&mut d, address, &format!("breakpoint stop {stop}"));
            if stop == WARMUP_STOPS {
                baseline = Some(measure());
            }
            // Leave the trap with an extra interrupt stop (disable, run, interrupt, re-arm), so
            // each loop adds stops of more than one kind to the port measurement.
            d.disable_breakpoint(id).expect("disable_breakpoint");
            d.resume().expect("resume with the breakpoint disabled");
            interrupt(&mut d);
            expect_interrupted(&mut d, &format!("the interrupt after stop {stop}"));
            d.enable_breakpoint(id).expect("enable_breakpoint");
            d.resume().expect("resume with the breakpoint enabled");
        }
        expect_breakpoint(&mut d, address, "the final breakpoint stop");
        let (names_before, refs_before) = baseline.expect("baseline measured");
        let (names_after, refs_after) = measure();
        d.remove_breakpoint(id).expect("remove_breakpoint");
        eprintln!("port names over {MEASURED_STOPS} stops: {names_before} -> {names_after}");
        eprintln!("send-right references over {MEASURED_STOPS} stops: {refs_before} -> {refs_after}");

        assert!(
            names_after as u64 <= names_before as u64 + PORT_TOLERANCE,
            "port names grew from {names_before} to {names_after} over {MEASURED_STOPS} stops"
        );
        assert!(
            refs_after <= refs_before + PORT_TOLERANCE,
            "send-right references grew from {refs_before} to {refs_after} over {MEASURED_STOPS} stops"
        );
    });
}

/// A failed exception reply is returned as an error by the next `wait`, the session keeps
/// stopped, and later stops are still reported.
#[test]
fn failed_exception_reply_is_reported_by_wait_and_later_stops_still_are()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        // The session owns the backend as `dyn Backend`; this handle reaches its hook.
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], backend);
        let address = marker_address(&mut d, &fixture, "tick_body");
        let id = d
            .add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        faults.fail_next_exception_reply();
        d.resume().expect("resume");
        expect_breakpoint(&mut d, address, "the first breakpoint stop");
        match d.wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(
                err.to_string().contains("reply"),
                "the error does not name the reply failure: {err}"
            ),
            Ok(event) => panic!("expected the reply failure from wait, got {event:?}"),
        }
        assert_eq!(d.state(), SessionState::Stopped, "an error from wait changed the state");
        d.disable_breakpoint(id).expect("disable_breakpoint");
        d.resume().expect("resume after the reply failure");

        // Stops are still reported: an interrupt, then a breakpoint stop again.
        interrupt(&mut d);
        expect_interrupted(&mut d, "the interrupt after the failed reply");
        d.enable_breakpoint(id).expect("enable_breakpoint");
        d.resume().expect("resume with the breakpoint enabled");
        expect_breakpoint(&mut d, address, "a breakpoint stop after the failed reply");
        d.remove_breakpoint(id).expect("remove_breakpoint");
    });
}

/// A failed `task_suspend` in the exception loop is not published as a stop (#191):
/// other threads could still be running while the engine reads memory or writes
/// traps. The next `wait` returns the failure, the session stays running, and the
/// held breakpoint stop is reported once a retried suspend succeeds, with the task
/// really suspended.
#[test]
fn failed_exception_suspend_is_reported_by_wait_not_as_a_stop()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], backend);
        let address = marker_address(&mut d, &fixture, "tick_body");
        d.add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        faults.fail_next_exception_suspend();
        d.resume().expect("resume");
        match d.wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(
                err.to_string().contains("task_suspend"),
                "the error does not name the failed task_suspend: {err}"
            ),
            Ok(event) => panic!("expected the task_suspend failure from wait, got {event:?}"),
        }
        assert_eq!(
            d.state(),
            SessionState::Running,
            "a stop without a task suspension was reported as stopped"
        );
        // The loop retries the suspend within milliseconds, so by now it may already
        // have published the held stop, with the task really suspended: checked below.
        expect_breakpoint(&mut d, address, "the breakpoint stop after the failed suspend");
        assert_eq!(d.state(), SessionState::Stopped);
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        assert!(
            counts.task.expect("task suspend count") > 0,
            "the reported stop does not hold the task suspended"
        );
    });
}

/// The error a failed exception-loop `task_suspend` makes `wait` return (#191).
fn expect_suspend_failure<E: std::fmt::Display>(result: Result<Option<StopEvent>, E>, what: &str)
{
    match result {
        Err(err) => assert!(
            err.to_string().contains("task_suspend"),
            "{what}: the error does not name the failed task_suspend: {err}"
        ),
        Ok(event) => panic!("{what}: expected the task_suspend failure from wait, got {event:?}"),
    }
}

/// A signal sent while the backend holds the target stopped is deferred until resume
/// (#79). A failed `task_suspend` for it after the resume must not lose it (#191): it is
/// still reported, once the task can be suspended.
#[test]
fn deferred_signal_survives_a_failed_exception_suspend()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], backend);
        let pid = d.pid().0;
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &pid.to_string()])
                .status()
                .expect("SIGUSR1")
                .success()
        );
        let b = d.session_mut().backend_mut();
        assert_eq!(
            b.wait(Some(Duration::from_millis(300))).expect("wait while stopped"),
            None,
            "a signal sent while stopped was published before resume"
        );
        faults.fail_next_exception_suspend();
        b.resume(&ResumeRequest::continue_all()).expect("resume");
        expect_suspend_failure(b.wait(Some(STOP_TIMEOUT)), "the deferred signal's suspend");
        match b.wait(Some(STOP_TIMEOUT)).expect("wait for the deferred signal") {
            Some(StopEvent::Signal { signal: 30, .. }) => {}
            other => panic!("the deferred SIGUSR1 was lost after the failed suspend, got {other:?}"),
        }
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        assert!(
            counts.task.expect("task suspend count") > 0,
            "the reported signal stop does not hold the task suspended"
        );
        assert!(!harness::process_is_gone(pid), "the suppressed SIGUSR1 was delivered");
    });
}

/// While the loop cannot suspend the task it holds the faulting thread with a
/// `thread_suspend` of its own and answers its exception (#191), so no unanswered reply
/// holds back an external SIGKILL: it still ends the target, and `wait` reports it.
#[test]
fn external_kill_ends_a_target_whose_exception_suspend_keeps_failing()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], backend);
        let pid = d.pid().0;
        let address = marker_address(&mut d, &fixture, "tick_body");
        d.add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        faults.fail_exception_suspends(u32::MAX);
        d.resume().expect("resume");
        expect_suspend_failure(d.wait(Some(STOP_TIMEOUT)), "the breakpoint's suspend");
        // Several retries fail meanwhile; none may publish a stop.
        assert_eq!(
            d.session_mut()
                .backend_mut()
                .wait(Some(Duration::from_millis(200)))
                .expect("wait while the suspend keeps failing"),
            None,
            "a retried stop was published although its task_suspend failed"
        );
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-9", &pid.to_string()])
                .status()
                .expect("SIGKILL")
                .success()
        );
        assert!(
            harness::wait_until_gone(pid, STOP_TIMEOUT),
            "the held exception kept SIGKILL from ending the target"
        );
        faults.fail_exception_suspends(0);
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
    });
}

/// A failed `task_suspend` for attach's private SIGSTOP reaches the caller at once as
/// that error (#191), not as a timeout of a SIGSTOP that never arrives, and the target
/// is released running.
#[test]
fn failed_exception_suspend_during_attach_fails_attach_promptly()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        backend.reply_fault_injector().fail_next_exception_suspend();
        let began = std::time::Instant::now();
        let result = ferros_core::Backend::attach(&mut backend, pid);
        let took = began.elapsed();
        let err = result.expect_err("attach succeeded although its task_suspend failed");
        assert!(err.to_string().contains("task_suspend"), "attach error: {err}");
        assert!(took < Duration::from_secs(3), "attach took {took:?} to fail");
        assert_runs(pid.0);
    });
}

/// A failed `task_suspend` for detach's private SIGSTOP reaches the caller at once as
/// that error (#191). The fallback `PT_DETACH` still releases the target running.
#[test]
fn failed_exception_suspend_during_detach_fails_detach_promptly()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        backend.reply_fault_injector().fail_next_exception_suspend();
        let began = std::time::Instant::now();
        let result = ferros_core::Backend::detach(&mut backend);
        let took = began.elapsed();
        let err = result.expect_err("detach reported success although its task_suspend failed");
        assert!(err.to_string().contains("task_suspend"), "detach error: {err}");
        assert!(took < Duration::from_secs(3), "detach took {took:?} to fail");
        assert_runs(pid.0);
    });
}

/// The `ps` state of `pid` (for example `S`, `R`, `T`, or `SX` while traced), empty if
/// it does not exist.
fn process_state(pid: u32) -> String
{
    let output = std::process::Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// `pid` is alive, not stopped, not traced, and neither its task nor any of its
/// threads is left suspended, sampled after a short while.
fn assert_runs(pid: u32)
{
    std::thread::sleep(Duration::from_millis(200));
    let stat = process_state(pid);
    assert!(!stat.is_empty(), "the target died");
    assert!(!stat.starts_with('T'), "the target was left stopped: {stat}");
    assert!(!stat.contains('X'), "the target was left traced: {stat}");
    let counts = diagnostics::process_suspend_counts(ferros_core::ProcessId(pid)).expect("suspend counts");
    assert_eq!(counts.task.expect("task suspend count"), 0, "the target was left suspended");
    for (index, thread) in counts.threads.expect("thread suspend counts").into_iter().enumerate() {
        let thread = thread.expect("thread suspend count");
        assert_eq!(thread.suspend_count, 0, "thread {index} was left suspended: {thread:?}");
    }
}

/// A client SIGSTOP held across failed `task_suspend`s (#191) is already answered, so it
/// is not the private SIGSTOP a detach handshake waits for. Dropping the backend
/// detaches through that handshake: the real private stop must satisfy it, so
/// `PT_DETACH` finds the tracee in its exception and the target is released untraced
/// and running. The injected failures last through the handshake.
#[test]
fn held_client_sigstop_does_not_satisfy_a_detach_handshake()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        ferros_core::Backend::resume(&mut backend, &ResumeRequest::continue_all()).expect("resume");
        faults.fail_exception_suspends(u32::MAX);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-STOP", &pid.0.to_string()])
                .status()
                .expect("SIGSTOP")
                .success()
        );
        expect_suspend_failure(
            ferros_core::Backend::wait(&mut backend, Some(STOP_TIMEOUT)),
            "the client SIGSTOP's suspend",
        );
        // The backend is still running: dropping it detaches from a running target.
        drop(backend);
        faults.fail_exception_suspends(0);
        assert_runs(pid.0);
    });
}

/// A detach that arms its handshake after the exception loop checked for one, but before
/// it took the lock for its suspend retry, still completes (#191): the loop sees the
/// handshake under that lock, takes no task suspend for the held client batch (the
/// interrupt's suspend handed to it is the detach's to balance), preserves the client
/// SIGSTOP for the handshake and releases the thread held for the failed suspend, so the
/// private SIGSTOP arrives. The hook pauses the loop's retry until the detach has sent
/// that SIGSTOP and balanced its suspends.
///
/// The kernel usually queues the private SIGSTOP's exception before the retry runs, so
/// this does not reproduce the hang of a fresh suspend taken before that exception
/// arrives; it covers the path the loop takes for it.
#[test]
fn detach_during_a_suspend_retry_still_receives_its_private_stop()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        ferros_core::Backend::resume(&mut backend, &ResumeRequest::continue_all()).expect("resume");
        faults.fail_exception_suspends(u32::MAX);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-STOP", &pid.0.to_string()])
                .status()
                .expect("SIGSTOP")
                .success()
        );
        expect_suspend_failure(
            ferros_core::Backend::wait(&mut backend, Some(STOP_TIMEOUT)),
            "the client SIGSTOP's suspend",
        );
        faults.hold_next_suspend_retry_for_detach();
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        while !faults.suspend_retry_is_held() {
            assert!(std::time::Instant::now() < deadline, "the loop did not retry its suspend");
            std::thread::sleep(Duration::from_millis(1));
        }
        // The interrupt hands its suspend to the paused retry; then suspends succeed.
        ferros_core::Backend::interrupt(&mut backend).expect("interrupt");
        faults.fail_exception_suspends(0);
        // The backend is still running: dropping it detaches through the handshake.
        let began = std::time::Instant::now();
        drop(backend);
        let took = began.elapsed();
        assert!(
            took < Duration::from_secs(3),
            "detach took {took:?}: its private SIGSTOP was held back by a fresh suspend"
        );
        assert_runs(pid.0);
    });
}

/// A failing suppression of a client signal held across failed `task_suspend`s (#191)
/// is retried with the suspend, but its thread is held with one `thread_suspend` only,
/// and each failure is reported once, not on every retry. Once both succeed, the signal
/// is reported as a stop.
#[test]
fn failing_signal_suppression_holds_its_thread_once()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], backend);
        let pid = d.pid().0;
        let b = d.session_mut().backend_mut();
        b.resume(&ResumeRequest::continue_all()).expect("resume");
        faults.fail_exception_suspends(u32::MAX);
        faults.fail_signal_suppressions(u32::MAX);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &pid.to_string()])
                .status()
                .expect("SIGUSR1")
                .success()
        );
        match b.wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(err.to_string().contains("suppress"), "first error: {err}"),
            Ok(event) => panic!("expected the suppression failure from wait, got {event:?}"),
        }
        expect_suspend_failure(b.wait(Some(STOP_TIMEOUT)), "the signal's suspend");
        // Many retries fail meanwhile; none may report again or hold the thread again.
        let quiet = b.wait(Some(Duration::from_millis(300)));
        assert!(
            matches!(quiet, Ok(None)),
            "a retry reported its failure again or published a stop: {quiet:?}"
        );
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        let threads = counts.threads.expect("thread suspend counts");
        let held: Vec<i32> = threads
            .into_iter()
            .map(|thread| thread.expect("thread suspend count").suspend_count)
            .collect();
        assert!(
            held.iter().all(|&count| count <= 1) && held.contains(&1),
            "the signalled thread is not held exactly once: {held:?}"
        );
        faults.fail_signal_suppressions(0);
        faults.fail_exception_suspends(0);
        let b = d.session_mut().backend_mut();
        match b.wait(Some(STOP_TIMEOUT)).expect("wait for the signal") {
            Some(StopEvent::Signal { signal: 30, .. }) => {}
            other => panic!("the held SIGUSR1 was not reported, got {other:?}"),
        }
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        assert!(
            counts.task.expect("task suspend count") > 0,
            "the stop does not hold the task"
        );
        for thread in counts.threads.expect("thread suspend counts") {
            assert_eq!(thread.expect("thread suspend count").suspend_count, 0, "a thread stayed held");
        }
        assert!(!harness::process_is_gone(pid), "the suppressed SIGUSR1 was delivered");
    });
}

/// A process killed from outside while the session holds its breakpoint stop still
/// dies before the client calls `wait`; the next `wait` reports it as killed.
#[test]
fn external_kill_while_a_breakpoint_stop_is_held_is_reported()
{
    let _serial = serial();
    harness::run_with_timeout(Duration::from_secs(60), |t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let pid = d.pid().0;
        let address = marker_address(&mut d, &fixture, "tick_body");
        d.add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        d.resume().expect("resume");
        expect_breakpoint(&mut d, address, "the breakpoint stop");

        let status = std::process::Command::new("/bin/kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("run /bin/kill");
        assert!(status.success(), "kill -9 {pid} failed: {status}");
        assert!(
            harness::wait_until_gone(pid, STOP_TIMEOUT),
            "pid {pid} survived kill -9 while its breakpoint stop was held"
        );
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
        assert_eq!(d.state(), SessionState::Exited);
    });
}

/// A signal deferred behind a breakpoint must not retain a kernel reply that
/// prevents a subsequent external SIGKILL from completing (#56).
#[test]
fn external_kill_after_a_signal_arrives_at_a_breakpoint_is_reported()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP, "--progress"]);
        let pid = d.pid().0;
        let address = marker_address(&mut d, &fixture, "tick_body");
        d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
        d.resume().expect("resume");
        expect_breakpoint(&mut d, address, "breakpoint");
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &pid.to_string()])
                .status()
                .expect("SIGUSR1")
                .success()
        );
        assert!(
            !d.wait_for_stdout("tick:", Duration::from_millis(400)),
            "fixture ran while stopped"
        );
        assert_eq!(d.state(), SessionState::Stopped);
        assert_eq!(d.read_registers().expect("registers").pc, address);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-9", &pid.to_string()])
                .status()
                .expect("SIGKILL")
                .success()
        );
        assert!(
            harness::wait_until_gone(pid, STOP_TIMEOUT),
            "tracee was not reaped before wait"
        );
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
        assert_eq!(d.state(), SessionState::Exited);
    });
}

/// Other signals must never let user code run while the breakpoint stop is held (#56).
#[test]
fn ordinary_signals_do_not_release_a_breakpoint_stop()
{
    let _serial = serial();
    harness::run(|t| {
        for other in "-USR2 -TERM -CONT -STOP".split_whitespace() {
            let fixture = fixture!("loop_counter");
            let mut d = t.launch_with_args(&fixture, &["25", "--progress"]);
            let pid = d.pid().0;
            let address = marker_address(&mut d, &fixture, "tick_body");
            d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
            d.resume().expect("resume");
            expect_breakpoint(&mut d, address, "the breakpoint stop");
            let status = std::process::Command::new("/bin/kill")
                .args([other, &pid.to_string()])
                .status()
                .expect("signal");
            assert!(status.success());
            assert!(
                !d.wait_for_stdout("tick:", Duration::from_millis(400)),
                "fixture ran after {other} during a breakpoint stop"
            );
            assert!(!harness::process_is_gone(pid), "fixture died after {other}");
            assert_eq!(d.state(), SessionState::Stopped);
            assert_eq!(d.read_registers().expect("registers").pc, address);
            d.kill().expect("kill");
        }
    });
}

/// Hold one worker's exception while admitting the second through its own gate.
/// The first worker's retained right must survive every step-over of the second.
#[test]
fn kept_exception_rights_do_not_grow_during_steps()
{
    let _serial = serial();
    harness::run(|t| {
        let control = ControlDir::new();
        let fixture = fixture!("step_races");
        let mut d = t.launch_with_args(&fixture, &[control.path().to_str().expect("control path")]);
        let address = marker_address(&mut d, &fixture, "step_site");
        d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
        control.release("run-0");
        d.resume().expect("run first worker");
        let held = expect_breakpoint(&mut d, address, "first worker");
        assert_eq!(d.wait(Some(Duration::ZERO)).expect("drain"), None);
        control.release("run-1");
        d.session_mut()
            .backend_mut()
            .resume(&ResumeRequest::continue_all().with_thread(held, ThreadAction::Suspend))
            .expect("retain first worker and run second");
        let stepped = expect_breakpoint(&mut d, address, "second worker");
        assert_ne!(held, stepped);
        let held_refs = diagnostics::send_right_count(u32::try_from(held.raw()).expect("port")).expect("held refs");
        assert!(held_refs >= 2, "no exception right retained for first worker: {held_refs}");
        let registers = d.session_mut().read_registers_for(stepped).expect("registers");
        let measure = || {
            (
                diagnostics::port_name_count().expect("names"),
                diagnostics::send_right_references().expect("references"),
            )
        };
        d.step_instruction(stepped).expect("warmup step");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("step"),
            Some(StopEvent::SingleStep { thread: stepped })
        );
        let (names, refs) = measure();
        for _ in 0..12 {
            d.session_mut().write_registers_for(stepped, &registers).expect("rewind");
            d.step_instruction(stepped).expect("step with retained exception");
            assert_eq!(
                d.wait(Some(STOP_TIMEOUT)).expect("step"),
                Some(StopEvent::SingleStep { thread: stepped })
            );
            assert_eq!(
                diagnostics::send_right_count(u32::try_from(held.raw()).expect("port")).expect("held refs"),
                held_refs,
                "first worker's retained exception right changed"
            );
        }
        let (after_names, after_refs) = measure();
        assert!(
            after_names as u64 <= names as u64 + PORT_TOLERANCE,
            "port names grew: {names} -> {after_names}"
        );
        assert!(
            after_refs <= refs + PORT_TOLERANCE,
            "send rights grew: {refs} -> {after_refs}"
        );
    });
}

/// A signal arriving at a breakpoint survives stepping over it and is then passed on.
#[test]
fn resume_with_signal_from_breakpoint_then_passes_pending_signal()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let address = marker_address(&mut d, &fixture, "tick_body");
        let id = d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
        d.resume().expect("resume");
        expect_breakpoint(&mut d, address, "breakpoint");
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &d.pid().0.to_string()])
                .status()
                .expect("signal")
                .success()
        );
        d.resume_with_signal().expect("step over breakpoint");
        match d.wait(Some(STOP_TIMEOUT)).expect("signal stop") {
            Some(StopEvent::Signal { signal: 30, .. }) => {}
            other => panic!("expected SIGUSR1, got {other:?}"),
        }
        d.remove_breakpoint(id).expect("remove breakpoint");
        d.resume_with_signal().expect("pass SIGUSR1");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("termination"),
            Some(StopEvent::Killed { signal: 30 })
        );
    });
}

/// The retry budget the give-up tests set, instead of the 5 s production default (#198).
const SHORT_RETRY_BUDGET: Duration = Duration::from_millis(1500);

/// The most `task_suspend` attempts a backed-off retry may make within
/// [`SHORT_RETRY_BUDGET`]: delays of 10, 20, 40, ... ms capped at 500 ms make about 9,
/// where fixed 10 ms retries would make about 150.
const BACKED_OFF_ATTEMPTS: u32 = 20;

/// Wait for the one error a give-up queues (#198).
fn expect_give_up<E: std::fmt::Display>(result: Result<Option<StopEvent>, E>)
{
    match result {
        Err(err) => {
            let text = err.to_string();
            assert!(
                text.contains("task_suspend") && text.contains("unrecoverable"),
                "the give-up error does not say the task cannot be suspended: {text}"
            );
        }
        Ok(event) => panic!("expected the task_suspend give-up from wait, got {event:?}"),
    }
}

/// Every thread of `pid` is held with exactly one `thread_suspend` and the task is not
/// suspended: the hold of a batch behind failed `task_suspend`s (#191).
fn assert_thread_held(pid: ferros_core::ProcessId)
{
    let counts = diagnostics::process_suspend_counts(pid).expect("suspend counts");
    assert_eq!(counts.task.expect("task suspend count"), 0, "the task was suspended");
    let held: Vec<i32> = counts
        .threads
        .expect("thread suspend counts")
        .into_iter()
        .map(|thread| thread.expect("thread suspend count").suspend_count)
        .collect();
    assert_eq!(held, [1], "the faulting thread is not held exactly once");
}

/// Launch `loop_counter` with progress output and a failing exception `task_suspend`
/// held at `tick_body` until the loop gives up (#198): checks that the retries back off
/// and that exactly one give-up error arrives. Returns the debuggee and the breakpoint.
fn give_up_at_a_breakpoint(t: &harness::TestContext) -> (Debuggee, ferros_macos::exception::ReplyFaultInjector, Address)
{
    let fixture = fixture!("loop_counter");
    let backend = MacOSBackend::new().expect("MacOSBackend::new");
    let faults = backend.reply_fault_injector();
    faults.set_suspend_retry_budget(SHORT_RETRY_BUDGET);
    let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP, "--progress"], backend);
    let address = marker_address(&mut d, &fixture, "tick_body");
    d.add_breakpoint(BreakpointRequest::Software { address })
        .expect("add_breakpoint at tick_body");
    faults.fail_exception_suspends(u32::MAX);
    let began = std::time::Instant::now();
    d.resume().expect("resume");
    expect_suspend_failure(d.wait(Some(STOP_TIMEOUT)), "the breakpoint's suspend");
    expect_give_up(d.wait(Some(STOP_TIMEOUT)));
    let took = began.elapsed();
    let attempts = faults.exception_suspend_attempts();
    assert!(
        took >= SHORT_RETRY_BUDGET && took < SHORT_RETRY_BUDGET + Duration::from_secs(2),
        "the loop gave up after {took:?}, not after its {SHORT_RETRY_BUDGET:?} budget"
    );
    assert!(
        (2..=BACKED_OFF_ATTEMPTS).contains(&attempts),
        "{attempts} task_suspend attempts in {took:?}: the retries did not back off"
    );
    (d, faults, address)
}

/// A `task_suspend` that keeps failing is retried with an exponential backoff, and
/// after the retry budget the loop gives up (#198): `wait` returns one error saying the
/// session is unrecoverable, then stays quiet, and the loop calls `task_suspend` no
/// more. The faulting thread stays held by its own `thread_suspend` and runs no user
/// code, and its exception is answered, so an external SIGKILL still ends the target.
#[test]
fn failing_exception_suspend_backs_off_then_gives_up_once()
{
    let _serial = serial();
    harness::run(|t| {
        let (mut d, faults, _) = give_up_at_a_breakpoint(t);
        let pid = d.pid();
        let attempts = faults.exception_suspend_attempts();
        let output = d.stdout().len();
        let quiet = d.session_mut().backend_mut().wait(Some(Duration::from_secs(1)));
        assert!(matches!(quiet, Ok(None)), "wait reported again after the give-up: {quiet:?}");
        assert_eq!(
            faults.exception_suspend_attempts(),
            attempts,
            "the loop kept calling task_suspend after it gave up"
        );
        assert_eq!(d.stdout().len(), output, "the held target ran user code");
        assert_thread_held(pid);
        assert_eq!(d.state(), SessionState::Running, "the give-up was reported as a stop");

        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-9", &pid.0.to_string()])
                .status()
                .expect("SIGKILL")
                .success()
        );
        assert!(
            harness::wait_until_gone(pid.0, STOP_TIMEOUT),
            "the held thread kept SIGKILL from ending the target after the give-up"
        );
        faults.fail_exception_suspends(0);
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
    });
}

/// After the loop gave up on `task_suspend` (#198), an interrupt still takes a task
/// suspend of its own and hands it to the loop, which publishes the held breakpoint stop
/// with it: a real suspend, so the stop holds the task and the session can kill it.
#[test]
fn interrupt_after_a_suspend_give_up_publishes_the_held_stop()
{
    let _serial = serial();
    harness::run(|t| {
        let (mut d, faults, address) = give_up_at_a_breakpoint(t);
        let attempts = faults.exception_suspend_attempts();
        interrupt(&mut d);
        expect_breakpoint(&mut d, address, "the held breakpoint stop after the give-up");
        assert_eq!(d.state(), SessionState::Stopped);
        assert_eq!(
            faults.exception_suspend_attempts(),
            attempts,
            "the loop called task_suspend again instead of taking the interrupt's"
        );
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        assert!(
            counts.task.expect("task suspend count") > 0,
            "the published stop does not hold the task suspended"
        );
        for thread in counts.threads.expect("thread suspend counts") {
            assert_eq!(thread.expect("thread suspend count").suspend_count, 0, "a thread stayed held");
        }
        faults.fail_exception_suspends(0);
        assert_eq!(d.kill().expect("kill"), StopEvent::Killed { signal: 9 });
    });
}

/// A failing `thread_suspend` of a thread whose exception is held behind a failed
/// `task_suspend` (#198) leaves that exception unanswered, and the thread waits in the
/// kernel instead of running. XNU does not complete a SIGKILL while a thread waits for its
/// exception reply, so an external `kill -9` does not end the target then: this is the
/// documented limit of the hold. The loop retries the freeze on each pass, and the first
/// that succeeds answers the exception, so the pending SIGKILL then takes effect.
#[test]
fn failed_thread_freeze_leaves_the_exception_unanswered_until_a_retry_holds_it()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP, "--progress"], backend);
        let pid = d.pid();
        let address = marker_address(&mut d, &fixture, "tick_body");
        d.add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        faults.fail_exception_suspends(u32::MAX);
        faults.fail_thread_freezes(u32::MAX);
        d.resume().expect("resume");
        expect_suspend_failure(d.wait(Some(STOP_TIMEOUT)), "the breakpoint's suspend");
        let output = d.stdout().len();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(d.stdout().len(), output, "the unanswered thread ran user code");
        let counts = diagnostics::process_suspend_counts(pid).expect("suspend counts");
        assert_eq!(counts.task.expect("task suspend count"), 0, "the task was suspended");
        let threads: Vec<_> = counts
            .threads
            .expect("thread suspend counts")
            .into_iter()
            .map(|thread| thread.expect("thread suspend count"))
            .collect();
        assert!(
            threads.len() == 1 && threads[0].suspend_count == 0,
            "the thread was held although its freeze failed: {threads:?}"
        );

        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-9", &pid.0.to_string()])
                .status()
                .expect("SIGKILL")
                .success()
        );
        assert!(
            !harness::wait_until_gone(pid.0, Duration::from_secs(1)),
            "SIGKILL ended a target whose exception is unanswered: the documented limit no longer holds"
        );
        faults.fail_thread_freezes(0);
        assert!(
            harness::wait_until_gone(pid.0, STOP_TIMEOUT),
            "a retried freeze did not answer the exception, so SIGKILL never took effect"
        );
        faults.fail_exception_suspends(0);
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
    });
}

/// After the loop gave up on `task_suspend` for a held client SIGSTOP (#198), a detach
/// still completes its handshake once the task can be suspended again: the loop takes a
/// real `task_suspend` for the private SIGSTOP instead of failing it with the give-up,
/// so the handshake completes promptly and the target is released untraced and running.
#[test]
fn detach_after_a_suspend_give_up_completes_its_handshake()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        faults.set_suspend_retry_budget(SHORT_RETRY_BUDGET);
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        ferros_core::Backend::resume(&mut backend, &ResumeRequest::continue_all()).expect("resume");
        faults.fail_exception_suspends(u32::MAX);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-STOP", &pid.0.to_string()])
                .status()
                .expect("SIGSTOP")
                .success()
        );
        expect_suspend_failure(
            ferros_core::Backend::wait(&mut backend, Some(STOP_TIMEOUT)),
            "the client SIGSTOP's suspend",
        );
        expect_give_up(ferros_core::Backend::wait(&mut backend, Some(STOP_TIMEOUT)));
        assert_thread_held(pid);
        let attempts = faults.exception_suspend_attempts();
        faults.fail_exception_suspends(0);
        // The backend is still running: dropping it detaches through the handshake.
        let began = std::time::Instant::now();
        drop(backend);
        let took = began.elapsed();
        assert!(took < Duration::from_secs(3), "detach took {took:?}");
        // Without a suspend of its own the private SIGSTOP fails with the give-up, and
        // only the fallback `PT_DETACH` releases the target.
        assert!(
            faults.exception_suspend_attempts() > attempts,
            "the detach handshake's private SIGSTOP took no task_suspend after the give-up"
        );
        assert_runs(pid.0);
    });
}

/// Wait (bounded) until the exception loop is paused by
/// `send_next_detach_stop_after_suspend_retry`.
fn wait_for_detach_stop_hold(faults: &ferros_macos::exception::ReplyFaultInjector)
{
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    while !faults.detach_stop_hold_is_paused() {
        assert!(std::time::Instant::now() < deadline, "the loop did not retry its suspend");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A detach that arms its handshake after the exception loop checked for one, but before
/// it took the lock for its suspend retry of a held client batch, still completes
/// (#198, G1): the loop sees the handshake under that lock and takes no fresh task
/// suspend, so the private SIGSTOP can arrive. Here the detach sends that SIGSTOP only
/// once the loop has finished the retry, so its exception cannot be queued before a
/// suspend the retry takes. A fresh suspend would hold the task, the thread could not
/// take the SIGSTOP, and the detach would time out after 5 s.
#[test]
fn detach_armed_before_a_suspend_retry_takes_no_fresh_suspend()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        ferros_core::Backend::resume(&mut backend, &ResumeRequest::continue_all()).expect("resume");
        faults.fail_exception_suspends(u32::MAX);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-STOP", &pid.0.to_string()])
                .status()
                .expect("SIGSTOP")
                .success()
        );
        expect_suspend_failure(
            ferros_core::Backend::wait(&mut backend, Some(STOP_TIMEOUT)),
            "the client SIGSTOP's suspend",
        );
        let awaited = faults.send_next_detach_stop_after_suspend_retry();
        wait_for_detach_stop_hold(&faults);
        // From the paused retry on, suspends succeed: without the recheck under the lock,
        // that retry takes a real one for the held client SIGSTOP.
        faults.fail_exception_suspends(0);
        let attempts = faults.exception_suspend_attempts();
        // The backend is still running: dropping it detaches through the handshake.
        let began = std::time::Instant::now();
        drop(backend);
        let took = began.elapsed();
        assert_eq!(
            awaited.try_recv(),
            Ok(true),
            "the detach did not send its private SIGSTOP late, or the handshake was already satisfied"
        );
        assert_eq!(
            faults.exception_suspend_attempts(),
            attempts + 1,
            "the held client batch took a task_suspend during the handshake, or the private SIGSTOP none"
        );
        assert!(
            took < Duration::from_secs(3),
            "detach took {took:?}: its private SIGSTOP was held back by a fresh suspend"
        );
        assert_runs(pid.0);
    });
}

/// A client SIGSTOP whose suppression failed while its `task_suspend` failed keeps its
/// reply outstanding (#191), but it was received before the detach armed its handshake,
/// so it is not that handshake's private stop (#198, G3). The detach sends its own
/// SIGSTOP only after the loop has processed the held SIGSTOP under the handshake: the
/// handshake must still be waiting then, receive its real private stop, and release the
/// target untraced and running. Taking the client SIGSTOP for the private one would
/// satisfy the handshake before the detach raised its own, and the detach's real SIGSTOP
/// would then land after `PT_DETACH`.
#[test]
fn unanswered_client_sigstop_does_not_satisfy_a_later_detach_handshake()
{
    let _serial = serial();
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[LONG_LOOP]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        ferros_core::Backend::resume(&mut backend, &ResumeRequest::continue_all()).expect("resume");
        // Suspends and suppressions fail until the detach arms its handshake, so the
        // client SIGSTOP is held with its reply outstanding until then.
        faults.fail_suspends_and_suppressions_until_detach();
        faults.fail_thread_freezes(u32::MAX);
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-STOP", &pid.0.to_string()])
                .status()
                .expect("SIGSTOP")
                .success()
        );
        match ferros_core::Backend::wait(&mut backend, Some(STOP_TIMEOUT)) {
            Err(err) => assert!(
                err.to_string().contains("task_suspend") || err.to_string().contains("suppress"),
                "first error: {err}"
            ),
            Ok(event) => panic!("expected the suspend or suppression failure from wait, got {event:?}"),
        }
        let awaited = faults.send_next_detach_stop_after_suspend_retry();
        wait_for_detach_stop_hold(&faults);
        faults.fail_thread_freezes(0);
        let began = std::time::Instant::now();
        drop(backend);
        let took = began.elapsed();
        assert_eq!(
            awaited.try_recv(),
            Ok(true),
            "the held client SIGSTOP satisfied the detach handshake before its private SIGSTOP was sent"
        );
        assert!(took < Duration::from_secs(3), "detach took {took:?}");
        assert_runs(pid.0);
    });
}

/// An exception raised just before the exception handler is torn down waits queued on
/// the exception port (#198, F6). The detach joins the loop, which holds the private
/// stop's record, and the loop answers what is queued before the port is destroyed. A
/// destroyed exception message is an unhandled exception: the kernel would turn this
/// single-step trap into a SIGTRAP and kill the released target. The hook parks the loop,
/// makes a running thread of `hot_loop` take a real single-step exception, then lets the
/// teardown go on.
#[test]
fn exception_queued_just_before_detach_teardown_is_answered()
{
    let _serial = serial();
    harness::run(|t| {
        let mut target = t.spawn(&fixture!("hot_loop"), &[&HOT_THREADS.to_string()]);
        let pid = target.pid();
        // The hook needs a thread that runs: attach only once every thread spins.
        wait_for_threads(pid, HOT_THREADS);
        let mut backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        ferros_core::Backend::attach(&mut backend, pid).expect("attach");
        faults.queue_exception_before_next_teardown();
        ferros_core::Backend::detach(&mut backend).expect("detach");
        assert!(
            faults.exception_was_queued_before_teardown(),
            "the hook did not queue a real exception before teardown"
        );
        drop(backend);
        std::thread::sleep(Duration::from_millis(200));
        let status = target.try_wait().expect("target status");
        assert!(
            status.is_none(),
            "the released target ended ({status:?}): the exception queued before teardown was destroyed unhandled"
        );
        assert_runs(pid.0);
    });
}

/// Spinning threads of the `hot_loop` targets below, main included.
const HOT_THREADS: usize = 4;

/// Wait (bounded) until the process `pid` has `count` threads.
fn wait_for_threads(pid: ferros_core::ProcessId, count: usize)
{
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    loop {
        let threads = diagnostics::thread_count(pid).unwrap_or(0);
        if threads >= count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the target has {threads} threads, not {count}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Threads of the backoff test's `hot_loop`: one for the first held signal, the others
/// raise one each during the measured window.
const BACKOFF_THREADS: usize = 8;

/// How long the backoff test lets a held batch fail before it measures: past the
/// doublings (10 + 20 + ... + 320 ms), so the delay has reached its 500 ms ceiling.
const BACKOFF_SETTLE: Duration = Duration::from_millis(1300);

/// Attempts after which the delay has reached its ceiling: 10 ms doubled six times
/// passes 500 ms, so the seventh retry waits the full delay.
const BACKOFF_SETTLED_ATTEMPTS: u32 = 7;

/// The window the backoff test measures, at the 500 ms ceiling.
const BACKOFF_WINDOW: Duration = Duration::from_millis(1000);

/// The most `task_suspend` attempts allowed within [`BACKOFF_WINDOW`]: two delays fit,
/// plus one for the window's edges. At least one must happen: arrivals must not
/// postpone the retry either.
const BACKOFF_WINDOW_ATTEMPTS: u32 = 3;

/// Exceptions that arrive while a batch is held behind failing `task_suspend`s do not
/// cut the backoff short (#198): the next attempt stays due one delay after the last.
/// Each thread of `hot_loop` gets a thread-directed SIGUSR1 in turn, a fresh exception
/// every ~140 ms, while the delay is 500 ms. Retrying at each arrival would make an
/// attempt per signal. Each signal's thread is still held at once, its own
/// `thread_suspend` holding it and its exception answered, so an external SIGKILL ends
/// the target.
#[test]
fn exceptions_arriving_during_a_suspend_backoff_do_not_advance_the_retry()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("hot_loop");
        let backend = MacOSBackend::new().expect("MacOSBackend::new");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[&BACKOFF_THREADS.to_string()], backend);
        let pid = d.pid();
        d.resume().expect("resume");
        wait_for_threads(pid, BACKOFF_THREADS);
        faults.fail_exception_suspends(u32::MAX);
        diagnostics::send_sigusr1_to_thread(pid, 0).expect("SIGUSR1 to the first thread");
        expect_suspend_failure(d.wait(Some(STOP_TIMEOUT)), "the first signal's suspend");
        std::thread::sleep(BACKOFF_SETTLE);
        // Under load the loop may lag: measure only once the doublings are spent.
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        while faults.exception_suspend_attempts() < BACKOFF_SETTLED_ATTEMPTS {
            assert!(
                std::time::Instant::now() < deadline,
                "the held batch did not retry its suspend"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let before = faults.exception_suspend_attempts();
        let began = std::time::Instant::now();
        let spacing = BACKOFF_WINDOW / u32::try_from(BACKOFF_THREADS).expect("thread count");
        for index in 1..BACKOFF_THREADS {
            diagnostics::send_sigusr1_to_thread(pid, index).expect("SIGUSR1 to a spinning thread");
            std::thread::sleep(spacing);
        }
        let attempts = faults.exception_suspend_attempts() - before;
        let took = began.elapsed();
        assert!(
            (1..=BACKOFF_WINDOW_ATTEMPTS).contains(&attempts),
            "{attempts} task_suspend attempts in {took:?} with a 500 ms backoff: arriving exceptions advanced the retry"
        );
        let quiet = d.session_mut().backend_mut().wait(Some(Duration::ZERO));
        assert!(matches!(quiet, Ok(None)), "a held signal was reported: {quiet:?}");

        // Every thread raised its signal and is held by its own suspend, the task not.
        // The last signal may still be on its way: wait for its hold.
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        let held = loop {
            let counts = diagnostics::process_suspend_counts(pid).expect("suspend counts");
            assert_eq!(counts.task.expect("task suspend count"), 0, "the task was suspended");
            let held: Vec<i32> = counts
                .threads
                .expect("thread suspend counts")
                .into_iter()
                .map(|thread| thread.expect("thread suspend count").suspend_count)
                .collect();
            if held == [1; BACKOFF_THREADS] || std::time::Instant::now() >= deadline {
                break held;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(held, [1; BACKOFF_THREADS], "a signalled thread is not held exactly once");

        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-9", &pid.0.to_string()])
                .status()
                .expect("SIGKILL")
                .success()
        );
        assert!(
            harness::wait_until_gone(pid.0, STOP_TIMEOUT),
            "a signal held during the backoff kept SIGKILL from ending the target"
        );
        faults.fail_exception_suspends(0);
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
    });
}

/// A bad memory access is a synchronous fault, not a held signal (#219): it stops as
/// [`StopEvent::Fault`], a forwarding continue after it is accepted and passes no signal
/// for it, and the retried access faults again at the same pc.
///
/// The fault is raised for real: stopped at `tick_body`, the thread's pc is moved to
/// address 0, so its next instruction fetch is an access to unmapped memory.
#[test]
fn access_fault_continue_is_not_rejected_as_a_signal()
{
    let _serial = serial();
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch(&fixture);
        let address = marker_address(&mut d, &fixture, "tick_body");
        let id = d
            .add_breakpoint(BreakpointRequest::Software { address })
            .expect("add_breakpoint at tick_body");
        d.resume().expect("resume");
        let thread = expect_breakpoint(&mut d, address, "the tick_body stop");
        d.remove_breakpoint(id).expect("remove_breakpoint");
        let mut registers = d.read_registers_for(thread).expect("read registers");
        registers.pc = Address::new(0);
        d.write_registers_for(thread, &registers).expect("move the pc to address 0");

        d.resume().expect("run into the fault");
        let fault = d.wait(Some(STOP_TIMEOUT)).expect("wait for the fault");
        // Forward first: on a backend that reports the fault as a signal it never held,
        // this continue is the call that was rejected.
        let pc = d.read_registers_for(thread).expect("registers at the fault").pc;
        d.resume_with_signal()
            .expect("a forwarding continue after a fault is not rejected as signal injection");
        let Some(StopEvent::Fault {
            thread: faulted, kind, ..
        }) = fault.clone()
        else {
            panic!("expected a memory access fault, got {fault:?}");
        };
        assert_eq!(faulted, thread, "the fault names another thread");
        assert_eq!(kind, FaultKind::MemoryAccess, "{fault:?}");

        let again = d.wait(Some(STOP_TIMEOUT)).expect("wait for the retried fault");
        assert!(
            matches!(again, Some(StopEvent::Fault { thread: t, kind: FaultKind::MemoryAccess, .. }) if t == thread),
            "the retried access did not fault again: {again:?}"
        );
        assert_eq!(
            d.read_registers_for(thread).expect("registers at the retried fault").pc,
            pc,
            "the retried fault is at another pc"
        );
    });
}

/// A line of the `soft_faults` fixture's C source, whose markers are not in its Rust stub.
fn soft_faults_location(marker: &str) -> ferros_core::BreakpointLocation
{
    let fixture = fixture!("soft_faults");
    let path = fixture.source_path().with_file_name("c").join("soft_faults.c");
    let source = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let line = ferros_tests::find_marker(&source, marker).unwrap_or_else(|err| panic!("marker {marker}: {err:?}"));
    ferros_core::BreakpointLocation::Line {
        file: "soft_faults.c".into(),
        line,
    }
}

/// `SIGUSR1` on macOS (`<sys/signal.h>`).
const SIGUSR1: i32 = 30;

/// A store through a null pointer is a memory access fault that holds no signal: a
/// forwarding continue is not rejected as signal injection, and the store faults again
/// at the same pc on the same thread (issue #244, the real-fault sibling of
/// `access_fault_continue_is_not_rejected_as_a_signal`).
#[test]
fn null_store_fault_continue_is_not_rejected_as_a_signal()
{
    let _serial = serial();
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("soft_faults"), &["--null-store"]);
        d.resume().expect("run into the null store");
        let fault = d.wait(Some(STOP_TIMEOUT)).expect("wait for the fault");
        let Some(StopEvent::Fault { thread, kind, .. }) = fault.clone() else {
            panic!("expected a memory access fault, got {fault:?}");
        };
        assert_eq!(kind, FaultKind::MemoryAccess, "{fault:?}");
        assert_eq!(fault.as_ref().and_then(StopEvent::deliverable_signal), None);
        let pc = d.read_registers_for(thread).expect("registers at the fault").pc;
        let store = d
            .set_breakpoint(soft_faults_location("null_store"))
            .expect("resolve the null_store line");
        let sites = d
            .logical_breakpoints()
            .iter()
            .find(|breakpoint| breakpoint.id == store)
            .map(|breakpoint| breakpoint.sites.clone())
            .expect("the logical breakpoint");
        d.delete_breakpoint(store).expect("delete the line breakpoint");
        let line = sites.first().expect("a site on the null_store line").position.line;
        assert_eq!(
            d.source_position(pc).expect("source position of the fault").line,
            line,
            "the fault is not on the null_store line"
        );

        d.resume_with_signal()
            .expect("a forwarding continue after a null store is not rejected as signal injection");
        let again = d.wait(Some(STOP_TIMEOUT)).expect("wait for the retried store");
        assert!(
            matches!(again, Some(StopEvent::Fault { thread: t, kind: FaultKind::MemoryAccess, .. }) if t == thread),
            "the retried store did not fault again: {again:?}"
        );
        assert_eq!(
            d.read_registers_for(thread).expect("registers at the retried fault").pc,
            pc,
            "the retried fault is at another pc"
        );
    });
}

/// One SIGUSR1, forwarded once, runs its handler exactly once: the handler's
/// breakpoint is hit once, and the program counts one run before exiting (issue #244).
#[test]
fn forwarded_soft_signal_runs_its_handler_once()
{
    let _serial = serial();
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("soft_faults"), &["--handler-once"]);
        let handler = d
            .set_breakpoint(soft_faults_location("in_handler"))
            .expect("break in the handler");
        d.resume().expect("run to the signal");
        let stop = d.wait(Some(STOP_TIMEOUT)).expect("wait for SIGUSR1");
        assert!(
            matches!(stop, Some(StopEvent::Signal { signal: SIGUSR1, .. })),
            "expected the SIGUSR1 stop, got {stop:?}"
        );
        d.resume_with_signal().expect("forward SIGUSR1");

        let mut hits = 0;
        let end = loop {
            match d.wait(Some(STOP_TIMEOUT)).expect("wait after forwarding") {
                Some(StopEvent::Breakpoint { .. }) => {
                    assert_eq!(d.breakpoint_hit().expect("a logical hit").id, handler);
                    hits += 1;
                    d.resume().expect("continue from the handler");
                }
                other => break other,
            }
        };
        assert!(matches!(end, Some(StopEvent::Exited { code: 0 })), "got {end:?}");
        assert_eq!(hits, 1, "the handler breakpoint was hit {hits} times");
        assert!(
            d.wait_for_stdout("handler runs: 1\n", STOP_TIMEOUT),
            "the program counted another number of handler runs: {:?}",
            d.stdout()
        );
    });
}

/// An `EXC_SOFTWARE` exception that is not a Unix signal (here `EXC_UNIX_ABORT`, raised
/// by the program itself) is reported as a stop holding no signal, so a forwarding
/// continue answers it instead of being rejected as signal injection (issue #244).
#[test]
fn software_exception_that_is_not_a_signal_is_continued_with_signal_forwarding()
{
    let _serial = serial();
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("soft_faults"), &["--software-exception"]);
        d.resume().expect("run to the exception");
        let stop = d.wait(Some(STOP_TIMEOUT)).expect("wait for the software exception");
        assert!(
            matches!(&stop, Some(StopEvent::Other { thread: Some(_), reason }) if reason.contains("EXC_SOFTWARE")),
            "expected a software exception stop holding no signal, got {stop:?}"
        );
        assert_eq!(stop.as_ref().and_then(StopEvent::deliverable_signal), None);
        d.resume_with_signal()
            .expect("a forwarding continue after a software exception is not rejected as signal injection");
        let end = d.wait(Some(STOP_TIMEOUT)).expect("wait for the exit");
        assert!(matches!(end, Some(StopEvent::Exited { code: 0 })), "got {end:?}");
        assert!(
            d.wait_for_stdout("exception reply: 0\n", STOP_TIMEOUT),
            "the exception was not answered with KERN_SUCCESS: {:?}",
            d.stdout()
        );
    });
}
