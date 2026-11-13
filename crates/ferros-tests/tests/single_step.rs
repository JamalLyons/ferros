//! Single-step and per-thread suspension in the macOS backend (T07, issue #33).
//!
//! These tests check the backend itself, below `Session::step_instruction` (covered in
//! `breakpoints.rs`), so they drive it through the session's escape hatch
//! (`d.session_mut().backend_mut()`): plant a trap with `write_memory`, run to it, put
//! the original bytes back, then resume with `ResumeRequest::step(thread)`
//! (that thread steps, every other thread stays suspended) and `wait`. Every test leaves
//! the backend *Stopped* or the process gone, so the session's drop works.
#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::{Duration, Instant};

use ferros_core::{Address, Backend, BackendError, ResumeRequest, Session, StopEvent, ThreadAction, ThreadId};
use ferros_macos::MacOSBackend;
use ferros_tests::arm64::{Flow, TRAP, flow};
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture};

/// How long a test waits for a stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Iterations passed to `loop_counter`, so it outlives every test (10 ms each).
const LONG_LOOP: &str = "100000";

/// `SIGUSR1` on macOS.
const SIGUSR1: i32 = 30;

/// Instructions stepped in the stepping tests.
const STEPS: usize = 8;

/// The session's backend.
fn backend(d: &mut Debuggee) -> &mut dyn Backend
{
    d.session_mut().backend_mut()
}

/// The pc of `thread`.
fn pc_of(d: &mut Debuggee, thread: ThreadId) -> Address
{
    backend(d)
        .read_registers(thread)
        .unwrap_or_else(|err| panic!("read the registers of thread {thread:?}: {err}"))
        .pc
}

/// The 32-bit instruction at `address`.
fn instruction_at(d: &mut Debuggee, address: Address) -> u32
{
    let bytes = backend(d)
        .read_memory(address, 4)
        .unwrap_or_else(|err| panic!("read the instruction at {address}: {err}"));
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

/// Write a trap at `address` and return the bytes it replaced.
fn plant_trap(d: &mut Debuggee, address: Address) -> Vec<u8>
{
    let original = backend(d).read_memory(address, TRAP.len()).expect("read the original bytes");
    backend(d).write_memory(address, &TRAP).expect("write the trap");
    original
}

/// Continue every thread and wait for the next stop, which must be a breakpoint stop at
/// `address`; returns its thread. Other threads' stops at the same trap, reported with no
/// resume in between, are drained and returned too.
fn run_to_trap(d: &mut Debuggee, address: Address) -> (ThreadId, Vec<ThreadId>)
{
    backend(d).resume(&ResumeRequest::continue_all()).expect("continue");
    let first = match backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for the trap") {
        Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
        other => panic!("expected a breakpoint stop at {address}, got {other:?}"),
    };
    let mut others = Vec::new();
    while let Some(event) = backend(d).wait(Some(Duration::ZERO)).expect("drain the stop") {
        match event {
            StopEvent::Breakpoint { thread, pc } if pc == address => others.push(thread),
            other => panic!("expected only breakpoint stops at {address}, got {other:?}"),
        }
    }
    (first, others)
}

/// Resume with `thread` stepping and every other thread suspended, and wait for the stop.
fn step(d: &mut Debuggee, thread: ThreadId) -> StopEvent
{
    backend(d).resume(&ResumeRequest::step(thread)).expect("resume with a step");
    backend(d)
        .wait(Some(STOP_TIMEOUT))
        .expect("wait for the step")
        .expect("a stop after the step")
}

/// Step `thread` [`STEPS`] times from where it is: every stop must be its `SingleStep`,
/// and each new pc must follow from the instruction just executed. Returns how many
/// steps were straight-line (pc + 4).
fn step_and_check(d: &mut Debuggee, thread: ThreadId, mut each: impl FnMut(&mut Debuggee, usize)) -> usize
{
    let mut straight = 0;
    for n in 1..=STEPS {
        let pc = pc_of(d, thread).value();
        let insn = instruction_at(d, Address::from(pc));
        let event = step(d, thread);
        assert_eq!(
            event,
            StopEvent::SingleStep { thread },
            "step {n} from {pc:#x} ({insn:#010x}) ended with another stop"
        );
        assert_eq!(
            backend(d).wait(Some(Duration::ZERO)).expect("wait(0) after a step"),
            None,
            "step {n} reported more than one stop"
        );
        let next = pc_of(d, thread).value();
        match flow(pc, insn) {
            Flow::Next => {
                assert_eq!(next, pc + 4, "step {n}: {insn:#010x} at {pc:#x} is no branch");
                straight += 1;
            }
            Flow::Direct(target) => assert_eq!(next, target, "step {n}: branch {insn:#010x} at {pc:#x}"),
            Flow::Conditional(target) => assert!(
                next == target || next == pc + 4,
                "step {n}: conditional branch {insn:#010x} at {pc:#x} went elsewhere"
            ),
            Flow::Indirect => assert_ne!(next, pc, "step {n}: {insn:#010x} at {pc:#x} did not move"),
        }
        each(d, n);
    }
    straight
}

/// The `ps` state of `pid` (for example `S`, `R`, `T` or `Z`), empty if it does not exist.
fn process_state(pid: u32) -> String
{
    let output = Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// `function_address` resolves a fixture function from the symbol table: a trap planted
/// there is hit with exactly that pc.
#[test]
fn function_address_resolves_a_fixture_function()
{
    harness::run(|t| {
        let fixture: Fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        let (thread, _) = run_to_trap(&mut d, tick);
        assert_eq!(pc_of(&mut d, thread), tick, "the stopped thread's pc is not the trap");
        backend(&mut d).write_memory(tick, &original).expect("restore");
    });
}

/// From the entry of `loop_counter::tick`, each step executes exactly one instruction and
/// is reported as a `SingleStep` of the stepped thread.
#[test]
fn step_executes_one_instruction_at_a_time()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        let (thread, _) = run_to_trap(&mut d, tick);
        backend(&mut d)
            .write_memory(tick, &original)
            .expect("restore the original bytes");

        let straight = step_and_check(&mut d, thread, |_, _| {});
        // The function starts with a straight-line prologue, so at least those steps
        // prove "one instruction" exactly.
        assert!(straight >= 4, "only {straight} of {STEPS} steps were straight-line");

        // A plain continue after the steps runs freely: no step is left armed.
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(300))).expect("wait"),
            None,
            "the target stopped again after a plain continue"
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt"),
            Some(StopEvent::Interrupted { .. })
        ));
    });
}

/// Stepping one worker of `threads` keeps every other thread where it is, even one that
/// holds an unanswered breakpoint stop of its own; continuing afterwards runs the fixture
/// to completion.
#[test]
fn stepping_one_thread_keeps_the_others_suspended()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        let mut d = t.launch(&fixture);
        let worker_body = function_address(&mut d, &fixture, "worker_body");
        let original = plant_trap(&mut d, worker_body);
        let (stepped, _) = run_to_trap(&mut d, worker_body);
        backend(&mut d)
            .write_memory(worker_body, &original)
            .expect("restore the original bytes");

        let threads = backend(&mut d).threads().expect("threads");
        assert!(threads.len() >= 3, "expected main and both workers, got {threads:?}");
        let others: Vec<(ThreadId, Address)> = threads
            .into_iter()
            .filter(|&thread| thread != stepped)
            .map(|thread| (thread, pc_of(&mut d, thread)))
            .collect();

        step_and_check(&mut d, stepped, |d, n| {
            for &(thread, pc) in &others {
                assert_eq!(pc_of(d, thread), pc, "thread {thread:?} moved during step {n}");
            }
        });

        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("threads finished", STOP_TIMEOUT),
            "the fixture did not finish after the steps"
        );
        match backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the exit") {
            Some(StopEvent::Exited { code: 0 }) => {}
            other => panic!("expected a clean exit, got {other:?}"),
        }
    });
}

/// Number of threads of process `pid`, as `ps -M` lists them (one line per thread after
/// the header).
fn thread_count(pid: u32) -> usize
{
    let output = Command::new("ps")
        .args(["-M", "-p", &pid.to_string()])
        .output()
        .expect("run ps -M");
    String::from_utf8_lossy(&output.stdout).lines().count().saturating_sub(1)
}

/// A step request naming a thread that has exited since the backend last listed the
/// threads fails with `NotFound` and leaves the target stopped, instead of suspending
/// every live thread with nothing to step.
#[test]
fn stepping_an_exited_thread_is_not_found()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        // Keep the process alive for a while after both workers exit.
        let mut d = t.launch_with_args(&fixture, &["20000"]);
        let pid = d.pid().0;
        let worker_body = function_address(&mut d, &fixture, "worker_body");
        let original = plant_trap(&mut d, worker_body);
        let (worker, _) = run_to_trap(&mut d, worker_body);
        backend(&mut d)
            .write_memory(worker_body, &original)
            .expect("restore the original bytes");
        // The backend's thread list now includes the worker.
        assert!(backend(&mut d).threads().expect("threads").contains(&worker));

        // Let both workers finish, then stop the process again without listing threads.
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("threads finished", STOP_TIMEOUT),
            "the workers never finished"
        );
        let deadline = Instant::now() + STOP_TIMEOUT;
        while thread_count(pid) > 1 {
            assert!(Instant::now() < deadline, "the worker threads never went away");
            std::thread::sleep(Duration::from_millis(10));
        }
        backend(&mut d).interrupt().expect("interrupt");
        match backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt") {
            Some(StopEvent::Interrupted { .. }) => {}
            other => panic!("expected the interrupt stop, got {other:?}"),
        }

        match backend(&mut d).resume(&ResumeRequest::step(worker)) {
            Err(BackendError::NotFound(_)) => {}
            other => panic!("stepping the exited worker {worker:?} must be NotFound, got {other:?}"),
        }
        // The failed resume left the target stopped: registers can still be read.
        let main = backend(&mut d).threads().expect("threads after the failed resume");
        assert_eq!(main.len(), 1, "only the main thread should remain: {main:?}");
        assert!(backend(&mut d).read_registers(main[0]).is_ok());
    });
}

/// Detaching after a step leaves an attached process running: it is not left stopped
/// and runs on to its own exit.
#[test]
fn detach_after_a_step_leaves_the_process_running()
{
    const SHORT_LOOP: &str = "300";
    const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

    harness::run_with_timeout(Duration::from_secs(60), |t| {
        let target = t.spawn(&fixture!("loop_counter"), &[SHORT_LOOP]);
        let pid = target.pid();
        let mut session = Session::new(Box::new(MacOSBackend::new().expect("MacOSBackend::new")));
        session.attach(pid).expect("attach");
        let main = *session.threads().expect("threads").first().expect("a thread");

        let backend = session.backend_mut();
        backend.resume(&ResumeRequest::step(main)).expect("resume with a step");
        let event = backend.wait(Some(STOP_TIMEOUT)).expect("wait for the step");
        assert_eq!(event, Some(StopEvent::SingleStep { thread: main }));

        session.detach().expect("detach after a step");
        drop(session);

        for _ in 0..5 {
            let state = process_state(pid.0);
            assert!(
                !state.is_empty() && !state.starts_with('T'),
                "the detached process is stopped or gone (state {state:?})"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // It is still this test's child, so it stays a zombie once it exits by itself.
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while !process_state(pid.0).starts_with('Z') {
            assert!(
                Instant::now() < deadline,
                "the detached process did not run to completion (state {})",
                process_state(pid.0)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(target);
    });
}

/// Killing a launched process right after a step stop, or dropping its session while a
/// step with suspended threads is in flight, leaves no process behind.
#[test]
fn kill_after_a_step_leaves_no_process()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        for in_flight in [false, true] {
            let mut d = t.launch(&fixture);
            let pid = d.pid().0;
            let worker_body = function_address(&mut d, &fixture, "worker_body");
            let original = plant_trap(&mut d, worker_body);
            let (stepped, _) = run_to_trap(&mut d, worker_body);
            backend(&mut d).write_memory(worker_body, &original).expect("restore");
            if in_flight {
                // The session's drop kills the target while the backend runs the step.
                backend(&mut d).resume(&ResumeRequest::step(stepped)).expect("step");
                drop(d);
            } else {
                assert_eq!(step(&mut d, stepped), StopEvent::SingleStep { thread: stepped });
                d.kill().expect("kill after a step stop");
                drop(d);
            }
            assert!(
                harness::wait_until_gone(pid, STOP_TIMEOUT),
                "pid {pid} survived (in flight: {in_flight})"
            );
        }
    });
}

/// A signal held for a thread that stays suspended stays held while another thread
/// steps (and across an interrupt that ends a resume with every thread suspended), and
/// can be passed on afterwards. Naming it for the suspended thread is refused.
#[test]
fn held_signal_of_a_suspended_thread_survives_a_step_of_another()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        let mut d = t.launch(&fixture);
        let pid = d.pid().0;
        let worker_body = function_address(&mut d, &fixture, "worker_body");
        let original = plant_trap(&mut d, worker_body);
        run_to_trap(&mut d, worker_body);
        backend(&mut d).write_memory(worker_body, &original).expect("restore");
        let main = *backend(&mut d).threads().expect("threads").first().expect("main thread");

        // Raised while every thread is stopped, taken by some thread when they run.
        let status = Command::new("/bin/kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .expect("run /bin/kill");
        assert!(status.success());
        assert_eq!(
            backend(&mut d)
                .wait(Some(Duration::from_millis(100)))
                .expect("wait while stopped"),
            None,
            "a signal sent while stopped was published before resume"
        );
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        let signalled = match backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the signal") {
            Some(StopEvent::Signal { thread, signal: SIGUSR1 }) => thread,
            other => panic!("expected the SIGUSR1 stop, got {other:?}"),
        };
        assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);

        // Step a worker other than the signalled thread: the main thread waits in join
        // for the suspended workers, so its step could never complete.
        let stepped = backend(&mut d)
            .threads()
            .expect("threads")
            .into_iter()
            .find(|&thread| thread != signalled && thread != main)
            .expect("a worker other than the signalled thread");

        let refused = backend(&mut d).resume(&ResumeRequest::step(stepped).with_signal(signalled, SIGUSR1));
        assert!(
            matches!(refused, Err(BackendError::Unsupported(_))),
            "a signal for a suspended thread was not refused: {refused:?}"
        );

        assert_eq!(step(&mut d, stepped), StopEvent::SingleStep { thread: stepped });

        // Keep every thread suspended and end that resume with an interrupt: the kept
        // exception is then held by no pending stop, and the next resume answers it.
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend))
            .expect("resume with every thread suspended");
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(200))).expect("wait"),
            None,
            "a thread ran although every thread was suspended"
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt"),
            Some(StopEvent::Interrupted { .. })
        ));

        // Still held: passing it on now is accepted, and SIGUSR1 terminates the fixture.
        backend(&mut d)
            .resume(&ResumeRequest::continue_all().with_signal(signalled, SIGUSR1))
            .expect("pass the held signal on");
        match backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the end") {
            Some(StopEvent::Killed { signal: SIGUSR1 }) => {}
            other => panic!("expected the fixture killed by SIGUSR1, got {other:?}"),
        }
    });
}

/// Suspension applies to existing threads, not threads created by a continuing one.
#[test]
fn new_threads_can_run_after_suspend_snapshot()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let address = function_address(&mut d, &fixture, "spawn_later");
        let original = plant_trap(&mut d, address);
        let (creator, _) = run_to_trap(&mut d, address);
        backend(&mut d).write_memory(address, &original).expect("restore");
        let existing = backend(&mut d).threads().expect("threads");
        assert!(existing.len() >= 2, "expected an existing worker: {existing:?}");
        let suspended: Vec<_> = existing
            .into_iter()
            .filter(|&id| id != creator)
            .map(|id| (id, pc_of(&mut d, id)))
            .collect();
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
            .expect("resume creator");
        assert!(d.wait_for_stdout("new worker ran", STOP_TIMEOUT), "new thread did not run");
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        for (id, pc) in suspended {
            assert_eq!(pc_of(&mut d, id), pc, "existing suspended thread moved");
        }
    });
}

/// Releasing an attached session after starting a step resumes every suspended thread.
/// Explicit detach requires a stopped session; drop performs its own stop and detach.
#[test]
fn detach_cleanup_after_starting_step_releases_suspended_threads()
{
    harness::run_with_timeout(Duration::from_secs(30), |t| {
        for explicit in [false, true] {
            let mut target = t.spawn(&fixture!("threads"), &["1000", "1000"]);
            let pid = target.pid();
            let deadline = Instant::now() + STOP_TIMEOUT;
            while thread_count(pid.0) < 3 {
                assert!(Instant::now() < deadline, "workers never started");
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
            session.attach(pid).expect("attach");
            let threads = session.threads().expect("threads");
            assert!(threads.len() >= 3, "expected multiple threads: {threads:?}");
            session.step_instruction(threads[1]).expect("start step");
            if explicit {
                session.interrupt().expect("interrupt step");
                assert!(session.wait(Some(STOP_TIMEOUT)).expect("stop").is_some());
                while session.wait(Some(Duration::ZERO)).expect("drain").is_some() {}
                session.detach().expect("detach after stop");
            }
            drop(session);
            let deadline = Instant::now() + STOP_TIMEOUT;
            loop {
                if let Some(status) = target.try_wait().expect("query detached child status") {
                    assert!(status.success(), "detached process failed (explicit: {explicit}): {status}");
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "attached process did not finish (explicit: {explicit}): {}",
                    process_state(pid.0)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    });
}

/// Race an interrupt against a requested step, accepting either documented winner.
#[test]
fn interrupt_races_a_real_instruction_step()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        let (thread, _) = run_to_trap(&mut d, tick);
        backend(&mut d).write_memory(tick, &original).expect("restore");
        backend(&mut d).resume(&ResumeRequest::step(thread)).expect("step");
        backend(&mut d).interrupt().expect("interrupt");
        let event = backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait");
        assert!(
            matches!(event, Some(StopEvent::SingleStep { thread: id }) if id == thread)
                || matches!(event, Some(StopEvent::Interrupted { .. })),
            "unexpected race outcome: {event:?}"
        );
        // Drain any event from the same stop before continuing, then ensure no step
        // exception appears after the next continue.
        while backend(&mut d).wait(Some(Duration::ZERO)).expect("drain").is_some() {}
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(300))).expect("wait"),
            None,
            "stray stop after cancellation"
        );
        backend(&mut d).interrupt().expect("interrupt after continue");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
    });
}

/// Deliver a signal while stopped and race it against a step. When the signal wins,
/// a subsequent plain continue must not leave hardware single-step enabled.
#[test]
fn signal_cancels_a_real_step_without_a_stray_step_stop()
{
    const TRIALS: usize = 20;
    harness::run_with_timeout(Duration::from_secs(60), |t| {
        let fixture = fixture!("loop_counter");
        for trial in 1..=TRIALS {
            let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
            let tick = function_address(&mut d, &fixture, "tick");
            let original = plant_trap(&mut d, tick);
            let (thread, _) = run_to_trap(&mut d, tick);
            backend(&mut d).write_memory(tick, &original).expect("restore");
            assert!(
                Command::new("/bin/kill")
                    .args(["-USR1", &d.pid().0.to_string()])
                    .status()
                    .expect("signal")
                    .success()
            );
            backend(&mut d).resume(&ResumeRequest::step(thread)).expect("start step");
            let event = backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("step or signal");
            match event {
                Some(StopEvent::SingleStep { thread: id }) if id == thread => continue,
                Some(StopEvent::Signal { signal: SIGUSR1, .. }) => {}
                other => panic!("unexpected step race outcome: {other:?}"),
            }
            // Suppress the held signal and resume freely. A stale step bit would stop
            // the target immediately with SingleStep instead of running the loop.
            assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);
            backend(&mut d)
                .resume(&ResumeRequest::continue_all())
                .expect("continue after signal");
            assert_eq!(
                backend(&mut d).wait(Some(Duration::from_millis(300))).expect("wait"),
                None,
                "stray stop after signal cancelled the step"
            );
            backend(&mut d).interrupt().expect("interrupt");
            assert!(matches!(
                backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
                Some(StopEvent::Interrupted { .. })
            ));
            eprintln!("signal won the step race on trial {trial} of {TRIALS}");
            return;
        }
        panic!("signal did not win the step race in {TRIALS} trials");
    });
}
