//! Mach exception plumbing of the macOS backend: simultaneous stops are queued, the
//! port rights of exception messages are released, detaching restores the target's
//! exception ports, and a failed exception reply or task suspend is reported without
//! ending stop reporting.
#![cfg(target_os = "macos")]

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ferros_core::{Address, BreakpointRequest, ResumeRequest, SessionState, StopEvent, ThreadAction, ThreadId};
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
