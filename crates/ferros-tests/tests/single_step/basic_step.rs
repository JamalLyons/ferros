//! Single-step basics: one instruction per step, suspended peers, exits and detach.

use std::time::{Duration, Instant};

use ferros_core::{Address, BackendError, ResumeRequest, StopEvent, ThreadAction, ThreadId};
use ferros_macos::diagnostics;
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture, harness};

use crate::helpers::*;

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
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
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
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
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

/// Steps run back to back do not wait on the exception loop's poll (issue #160).
///
/// Each resume hands the loop a `Continue` while it holds the stop. Before the fix the
/// loop noticed that only when its 10 ms receive timed out, so every step took at least
/// 10 ms: a few hundred steps through the unwinder took seconds, and the stepping tests
/// over a caught panic timed out on a slower CI runner. Two hundred steps in less than a
/// second leaves a wide margin either way (they take about 2 s at 10 ms each).
#[test]
fn back_to_back_steps_do_not_wait_for_the_exception_loop_poll()
{
    const RUNS: u32 = 200;
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
        let (thread, _) = run_to_trap(&mut d, tick);
        backend(&mut d)
            .write_memory(tick, &original)
            .expect("restore the original bytes");

        let started = Instant::now();
        for n in 1..=RUNS {
            let event = step(&mut d, thread);
            assert_eq!(event, StopEvent::SingleStep { thread }, "step {n} ended with another stop");
        }
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(1), "{RUNS} instruction steps took {elapsed:?}");
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
        // The other worker stops at the trap too and holds that stop while `stepped` steps.
        let (stepped, _) = run_to_both_workers(&mut d, worker_body);
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
        let (worker, _) = run_to_both_workers(&mut d, worker_body);
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
        let mut session = ferros_macos::new_session().expect("MacOSBackend::new");
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
            let (stepped, _) = run_to_both_workers(&mut d, worker_body);
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
        let worker_body = function_address(&mut d, &fixture, "worker_body");
        let original = plant_trap(&mut d, worker_body);
        let (stepped, signalled) = run_to_both_workers(&mut d, worker_body);
        let continue_others = ResumeRequest::continue_all().with_thread(stepped, ThreadAction::Suspend);
        backend(&mut d).write_memory(worker_body, &original).expect("restore");

        // Queue SIGUSR1 while stopped and keep the other worker at worker_body until its step.
        diagnostics::send_sigusr1(signalled).expect("send SIGUSR1 to the second worker");
        assert_eq!(
            backend(&mut d)
                .wait(Some(Duration::from_millis(100)))
                .expect("wait while stopped"),
            None,
            "a signal sent while stopped was published before resume"
        );
        backend(&mut d).resume(&continue_others).expect("resume to the signal");
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the signal"),
            Some(StopEvent::Signal {
                thread: signalled,
                signal: SIGUSR1,
            })
        );
        assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);

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
