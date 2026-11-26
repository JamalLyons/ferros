//! Birth hold: a new thread stays at the pthread entry trap; failed-suspend paths.

use std::time::{Duration, Instant};

use ferros_core::{BackendError, ResumeRequest, StopEvent, ThreadAction};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::arm64::{Flow, flow};
use ferros_tests::symbols::function_address;
use ferros_tests::{fixture, harness};

use super::helpers::*;

/// The original regression: a thread created after a suspend snapshot used to run.
/// It now stays at the pthread entry until a later continue, which then finishes the process.
#[test]
fn new_threads_can_run_after_suspend_snapshot()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        // `hold_new_thread` checks that the existing threads stayed suspended and the
        // newborn is on a trap.
        let _ = hold_new_thread(&mut d, &fixture);
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "a later continue did not let the held thread finish, stdout: {}",
            d.stdout()
        );
        assert!(
            d.stdout().contains("new worker ran"),
            "the held thread never ran: {}",
            d.stdout()
        );
    });
}

/// Continuing only the held thread, while the default stays `Suspend`, executes the
/// original pthread instruction and reaches application code. The trap stays armed for
/// any later newborn, so this is the internal step off the site.
#[test]
fn held_new_thread_runs_original_instruction_under_suspend()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (_creator, suspended, newborn) = hold_new_thread(&mut d, &fixture);
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(newborn, ThreadAction::Continue))
            .expect("continue the newborn");
        assert!(
            d.wait_for_stdout("new worker ran", STOP_TIMEOUT),
            "stepping off the birth trap did not run the new thread, stdout: {}",
            d.stdout()
        );
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(50))).expect("wait"),
            None,
            "stepping off the birth trap reported a stop"
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        assert_suspended_still(&mut d, &suspended);
    });
}

/// A requested single-step of the held thread executes the original `stp` and stops on
/// the following `bl`, with the birth trap armed again.
#[test]
fn single_step_of_held_new_thread_executes_pthread_prologue()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (_creator, suspended, newborn) = hold_new_thread(&mut d, &fixture);
        let pc = pc_of(&mut d, newborn);
        let event = step(&mut d, newborn);
        assert_eq!(event, StopEvent::SingleStep { thread: newborn });
        assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);
        let next = pc_of(&mut d, newborn);
        assert_eq!(
            next.value(),
            pc.value() + 4,
            "the step did not execute exactly one instruction"
        );
        assert_eq!(raw_instruction_at(&d, pc), BRK, "the birth trap was not restored");
        let branch = instruction_at(&mut d, next);
        assert_eq!(
            branch & 0xfc00_0000,
            0x9400_0000,
            "the instruction after the prologue is not bl: {branch:#010x}"
        );
        assert_suspended_still(&mut d, &suspended);
        assert!(!d.stdout().contains("new worker ran"), "the single-step ran the worker body");
    });
}

/// Creating a thread while another thread is mid single-step still holds the newborn.
#[test]
fn new_thread_created_during_step_stays_at_the_trap()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (creator, suspended) = stop_before_second_thread(&mut d, &fixture);
        let (stepped, stepped_pc) = suspended[0];
        let held: Vec<_> = suspended.iter().copied().filter(|(id, _)| *id != stepped).collect();
        backend(&mut d)
            .resume(
                &ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(creator, ThreadAction::Continue)
                    .with_thread(stepped, ThreadAction::Step),
            )
            .expect("resume creator and step another thread");
        assert!(
            !d.wait_for_stdout("new worker ran", BIRTH_QUIET),
            "new thread ran during a step resume, stdout: {}",
            d.stdout()
        );
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(50))).expect("wait"),
            None,
            "the step or the birth trap stopped the process early"
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        assert_suspended_still(&mut d, &held);
        let known: Vec<_> = suspended.iter().map(|(id, _)| *id).chain(std::iter::once(creator)).collect();
        let newborn = backend(&mut d)
            .threads()
            .expect("threads")
            .into_iter()
            .find(|id| !known.contains(id))
            .expect("the new thread was not enumerated");
        let pc = pc_of(&mut d, newborn);
        assert_eq!(raw_instruction_at(&d, pc), BRK);
        // The stepped thread was blocked in its sleep, so the step should not have landed.
        assert_eq!(pc_of(&mut d, stepped), stepped_pc, "the stepped sleeper moved");
    });
}

/// A failed `task_suspend` for a completed single-step must not let the stepped
/// thread run on (#191): `wait` reports the failure, then the step, with the thread on
/// the instruction after the one it stepped.
#[test]
fn failed_exception_suspend_keeps_a_completed_step()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], debugger);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
        let (thread, _) = run_to_trap(&mut d, tick);
        backend(&mut d)
            .write_memory(tick, &original)
            .expect("restore the original bytes");
        let insn = instruction_at(&mut d, tick);
        assert_eq!(flow(tick.value(), insn), Flow::Next, "tick does not start straight-line");

        faults.fail_next_exception_suspend();
        backend(&mut d)
            .resume(&ResumeRequest::step(thread))
            .expect("resume with a step");
        match backend(&mut d).wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(err.to_string().contains("task_suspend"), "wait error: {err}"),
            Ok(event) => panic!("expected the task_suspend failure from wait, got {event:?}"),
        }
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_secs(2))).expect("wait for the step"),
            Some(StopEvent::SingleStep { thread }),
            "the completed step was lost after the failed suspend"
        );
        assert_eq!(
            pc_of(&mut d, thread).value(),
            tick.value() + 4,
            "the stepped thread ran past its step"
        );
        assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);
    });
}

/// An interrupt while a failed `task_suspend` holds a completed step (#191) reports
/// that step as its stop, with the task really suspended, not a separate `Interrupted`
/// that lets the held step be published after the next resume, for a resume it does
/// not belong to.
#[test]
fn interrupt_reports_a_step_held_by_a_failing_exception_suspend()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], debugger);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
        let (thread, _) = run_to_trap(&mut d, tick);
        backend(&mut d)
            .write_memory(tick, &original)
            .expect("restore the original bytes");
        assert_eq!(flow(tick.value(), instruction_at(&mut d, tick)), Flow::Next);

        faults.fail_exception_suspends(u32::MAX);
        backend(&mut d)
            .resume(&ResumeRequest::step(thread))
            .expect("resume with a step");
        match backend(&mut d).wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(err.to_string().contains("task_suspend"), "wait error: {err}"),
            Ok(event) => panic!("expected the task_suspend failure from wait, got {event:?}"),
        }
        // Retries keep failing: the step stays held, unpublished.
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(100))).expect("wait"),
            None,
            "a stop was published although its task_suspend keeps failing"
        );
        backend(&mut d).interrupt().expect("interrupt");
        let stop = backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt");
        faults.fail_exception_suspends(0);
        assert_eq!(
            stop,
            Some(StopEvent::SingleStep { thread }),
            "the interrupt did not report the held step"
        );
        assert_eq!(pc_of(&mut d, thread).value(), tick.value() + 4, "the stepped thread ran on");
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        assert!(
            counts.task.expect("task suspend count") > 0,
            "the stop does not hold the task"
        );
        assert_eq!(
            diagnostics::thread_suspend_count(thread).expect("thread suspend count"),
            0,
            "the thread held for the failed suspend was not released"
        );
        assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);

        // No stale step from before the interrupt reaches the next resume.
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("resume");
        assert_eq!(
            backend(&mut d)
                .wait(Some(Duration::from_millis(300)))
                .expect("wait after resume"),
            None,
            "a stale stop was published for the resume after the interrupt"
        );
        backend(&mut d).interrupt().expect("interrupt again");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt"),
            Some(StopEvent::Interrupted { .. })
        ));
        // The resume really let the target run: it calls `tick` again.
        plant_trap(&mut d, tick);
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
        let (again, _) = run_to_trap(&mut d, tick);
        assert_eq!(again, thread, "another thread reached tick");
    });
}

/// An interrupt that lands after the exception loop received a completed step but
/// before its own `task_suspend` failed (#191) is reported as `Interrupted`, and the
/// step stays held. A resume after that interrupt must not start while the step is
/// held unpublished: it is refused (`StopPending`), and the next `wait` reports the
/// step, with the task suspended. Otherwise the step would be published after the
/// resume, for a resume it does not belong to.
#[test]
fn resume_after_an_interrupt_is_refused_while_a_step_is_held()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], debugger);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = plant_trap(&mut d, tick);
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
        let (thread, _) = run_to_trap(&mut d, tick);
        backend(&mut d)
            .write_memory(tick, &original)
            .expect("restore the original bytes");
        assert_eq!(flow(tick.value(), instruction_at(&mut d, tick)), Flow::Next);

        // The loop receives the step and pauses before its task_suspend.
        faults.pause_next_stop_publication();
        backend(&mut d)
            .resume(&ResumeRequest::step(thread))
            .expect("resume with a step");
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !faults.stop_publication_is_paused() {
            assert!(Instant::now() < deadline, "the step was not received");
            std::thread::sleep(Duration::from_millis(1));
        }
        backend(&mut d).interrupt().expect("interrupt");
        faults.fail_exception_suspends(u32::MAX);
        faults.release_stop_publication();
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt"),
            Some(StopEvent::Interrupted { .. })
        ));
        match backend(&mut d).wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(err.to_string().contains("task_suspend"), "wait error: {err}"),
            Ok(event) => panic!("expected the task_suspend failure from wait, got {event:?}"),
        }

        let refused = backend(&mut d).resume(&ResumeRequest::continue_all());
        faults.fail_exception_suspends(0);
        assert_eq!(
            refused,
            Err(BackendError::StopPending),
            "a resume started while a completed step was held unpublished"
        );
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the held step"),
            Some(StopEvent::SingleStep { thread }),
            "the refused resume did not leave the held step to wait"
        );
        assert_eq!(pc_of(&mut d, thread).value(), tick.value() + 4, "the stepped thread ran on");
        let counts = diagnostics::process_suspend_counts(d.pid()).expect("suspend counts");
        assert!(
            counts.task.expect("task suspend count") > 0,
            "the stop does not hold the task"
        );
        assert_eq!(
            diagnostics::thread_suspend_count(thread).expect("thread suspend count"),
            0,
            "the thread held for the failed suspend was not released"
        );
        assert_eq!(backend(&mut d).wait(Some(Duration::ZERO)).expect("drain"), None);

        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("resume");
        assert_eq!(
            backend(&mut d)
                .wait(Some(Duration::from_millis(300)))
                .expect("wait after resume"),
            None,
            "a stale stop was published for the resume"
        );
        backend(&mut d).interrupt().expect("interrupt again");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt"),
            Some(StopEvent::Interrupted { .. })
        ));
        plant_trap(&mut d, tick);
        // Only loop_counter's main thread calls tick, so no peer can reach this trap.
        let (again, _) = run_to_trap(&mut d, tick);
        assert_eq!(again, thread, "another thread reached tick");
    });
}

/// A failed `task_suspend` for a birth step-off must not skip its bookkeeping (#191):
/// the entry trap is armed again and the creator, frozen for the step, is let go, so it
/// starts the second thread, which the trap then holds.
#[test]
fn failed_exception_suspend_keeps_a_birth_step_off()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[], debugger);
        let (creator, suspended, newborn) = stop_before_second_spawn(&mut d, &fixture);
        faults.fail_next_exception_suspend();
        backend(&mut d)
            .resume(
                &ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(creator, ThreadAction::Continue)
                    .with_thread(newborn, ThreadAction::Continue),
            )
            .expect("continue creator and newborn");
        match backend(&mut d).wait(Some(STOP_TIMEOUT)) {
            Err(err) => assert!(err.to_string().contains("task_suspend"), "wait error: {err}"),
            Ok(event) => panic!("expected the task_suspend failure from wait, got {event:?}"),
        }
        assert!(
            !d.wait_for_stdout("second worker ran", BIRTH_QUIET),
            "the second thread ran through the birth entry, stdout: {}",
            d.stdout()
        );
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(50))).expect("wait"),
            None,
            "a birth stop was delivered to the client"
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        assert_suspended_still(&mut d, &suspended);
        let known: Vec<_> = suspended
            .iter()
            .map(|(id, _)| *id)
            .chain([creator, newborn])
            .collect::<Vec<_>>();
        let second_thread = backend(&mut d)
            .threads()
            .expect("threads")
            .into_iter()
            .find(|id| !known.contains(id))
            .expect("the creator never started the second thread: it stayed frozen for the step-off");
        let second_pc = pc_of(&mut d, second_thread);
        assert_eq!(
            raw_instruction_at(&d, second_pc),
            BRK,
            "the second thread is not held on a re-armed trap"
        );
    });
}

/// Releases a birth step-off hold if the test panics while the entry is restored.
struct ReleaseHold<'a>(&'a ferros_macos::exception::ReplyFaultInjector);

impl Drop for ReleaseHold<'_>
{
    fn drop(&mut self)
    {
        self.0.release_birth_step_off();
    }
}

/// A creator resumed with a held newborn must not start another thread through the
/// instruction that step-off temporarily restores.
#[test]
fn second_thread_created_during_birth_step_off_stays_at_the_trap()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[], debugger);
        let (creator, suspended, newborn) = stop_before_second_spawn(&mut d, &fixture);
        faults.hold_next_birth_step_off();
        let _release = ReleaseHold(&faults);
        backend(&mut d)
            .resume(
                &ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(creator, ThreadAction::Continue)
                    .with_thread(newborn, ThreadAction::Continue),
            )
            .expect("continue creator and newborn");
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !faults.birth_step_off_is_held() {
            assert!(
                Instant::now() < deadline,
                "the birth step-off was not held, stdout: {}",
                d.stdout()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // The entry instruction is restored and the stepping thread has already
        // stopped. The creator was told to `Continue`, so only the step-off freeze
        // gives it a suspend of its own; without one it could start the second worker
        // through the open entry whenever it is scheduled.
        assert!(
            diagnostics::thread_suspend_count(creator).expect("creator suspend count") > 0,
            "the creator is not frozen while the birth entry is restored"
        );
        for (id, _) in &suspended {
            assert!(
                diagnostics::thread_suspend_count(*id).expect("suspend count") > 0,
                "existing thread {id:?} is not suspended while the birth entry is restored"
            );
        }
        assert!(
            !d.stdout().contains("second worker ran"),
            "the second thread ran while the birth entry was restored: {}",
            d.stdout()
        );
        faults.release_birth_step_off();
        assert!(
            !d.wait_for_stdout("second worker ran", BIRTH_QUIET),
            "the second thread ran after the trap was restored, stdout: {}",
            d.stdout()
        );
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(50))).expect("wait"),
            None,
            "the second birth trap was delivered to the client"
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        assert_suspended_still(&mut d, &suspended);
        let known: Vec<_> = suspended
            .iter()
            .map(|(id, _)| *id)
            .chain([creator, newborn])
            .collect::<Vec<_>>();
        let second_thread = backend(&mut d)
            .threads()
            .expect("threads")
            .into_iter()
            .find(|id| !known.contains(id))
            .expect("the second thread was not enumerated");
        let second_pc = pc_of(&mut d, second_thread);
        assert_eq!(
            raw_instruction_at(&d, second_pc),
            BRK,
            "the second thread is not held on a trap"
        );
    });
}
