//! Mach exception plumbing of the macOS backend: simultaneous stops are queued, the
//! port rights of exception messages are released, detaching restores the target's
//! exception ports, and a failed exception reply is reported without ending stop
//! reporting.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use ferros_core::symbols::ImageDescriptor;
use ferros_core::{Address, BreakpointRequest, Session, SessionState, StopEvent, SymbolCache, ThreadId};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::{Fixture, fixture};

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

/// Address of the first instruction of the line marked `marker` in `fixture`'s source.
///
/// The session has no `file:line` breakpoints, so this symbolicates (with the fixture's
/// dSYM) every instruction of the main executable's text region and picks the lowest address attributed to that
/// line (by any frame, so a call site of an inlined function counts). The offset from
/// the image's load address is cached, because later launches only differ by the slide.
fn marker_address(d: &mut Debuggee, fixture: &Fixture, marker: &str) -> Address
{
    static OFFSETS: OnceLock<Mutex<HashMap<(String, String), u64>>> = OnceLock::new();

    let images = d.images().expect("images");
    let main = images
        .iter()
        .find(|image| image.path == fixture.path())
        .expect("the main executable image");
    let load_address = main.load_address.value();
    let key = (fixture.name().to_owned(), marker.to_owned());
    let offsets = OFFSETS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(offset) = offsets.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return Address::from(load_address + offset);
    }

    let line = fixture.marker_line(marker);
    let source_name = format!("{}.rs", fixture.name());
    let regions = d.memory_regions().expect("memory_regions");
    let text = regions
        .iter()
        .find(|region| region.contains(main.load_address) && region.is_executable())
        .unwrap_or_else(|| panic!("no executable region holds the image at {}", main.load_address));
    // The executable supplies the runtime layout; its matching dSYM is discovered.
    let mut symbols = SymbolCache::new();
    symbols
        .load_image(ImageDescriptor {
            path: fixture.path().to_path_buf(),
            load_address,
        })
        .unwrap_or_else(|err| panic!("load the fixture's symbols: {err}"));

    let found = (text.start.value()..text.end.value()).step_by(4).find(|&address| {
        symbols.symbolicate(Address::from(address)).is_some_and(|symbolication| {
            symbolication.frames.iter().any(|frame| {
                frame
                    .location
                    .as_ref()
                    .is_some_and(|location| location.line == Some(line) && location.file.ends_with(&source_name))
            })
        })
    });
    let address = found.unwrap_or_else(|| panic!("no instruction for {source_name}:{line} (marker {marker})"));
    offsets
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, address - load_address);
    Address::from(address)
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

        let mut session = Session::new(Box::new(MacOSBackend::new().expect("MacOSBackend::new")));
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

/// Exercise stepping while another thread's breakpoint exception remains held. The
/// simultaneous stop is a scheduling race, so retry rather than pretend it is guaranteed.
#[test]
fn kept_exception_rights_do_not_grow_during_steps()
{
    let _serial = serial();
    harness::run_with_timeout(Duration::from_secs(120), |t| {
        let fixture = fixture!("threads");
        for _ in 0..SIMULTANEOUS_TRIALS {
            let mut d = t.launch(&fixture);
            let address = marker_address(&mut d, &fixture, "worker_body");
            d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
            d.resume().expect("resume");
            let first = expect_breakpoint(&mut d, address, "first worker");
            let second = d.wait(Some(Duration::ZERO)).expect("drain");
            if !matches!(second, Some(StopEvent::Breakpoint { thread, pc }) if thread != first && pc == address) {
                continue;
            }
            let registers = d.session_mut().read_registers_for(first).expect("breakpoint registers");
            assert_eq!(registers.pc, address);
            let measure = || {
                (
                    diagnostics::port_name_count().expect("names"),
                    diagnostics::send_right_references().expect("references"),
                )
            };
            d.step_instruction(first).expect("warmup step");
            assert_eq!(
                d.wait(Some(STOP_TIMEOUT)).expect("step"),
                Some(StopEvent::SingleStep { thread: first })
            );
            let (names, refs) = measure();
            for _ in 0..12 {
                // Revisit the trapped instruction to exercise repeated step-overs.
                d.session_mut().write_registers_for(first, &registers).expect("rewind");
                d.step_instruction(first).expect("step with other exception kept");
                assert_eq!(
                    d.wait(Some(STOP_TIMEOUT)).expect("step"),
                    Some(StopEvent::SingleStep { thread: first })
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
            return;
        }
        panic!("no simultaneous breakpoint stops in {SIMULTANEOUS_TRIALS} trials");
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
