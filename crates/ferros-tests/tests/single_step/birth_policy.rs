//! Birth policy: breakpoints on the entry, workqueue threads, detach, drop and kill.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ferros_core::{Address, BreakpointRequest, ResumeRequest, StopEvent, ThreadAction};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::arm64::TRAP;
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture};

use crate::helpers::*;

/// A resume that starts before dyld has published pthread still holds the new thread.
#[test]
fn thread_created_before_pthread_is_published_stays_at_the_trap()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let images = backend(&mut d).images().expect("images");
        assert!(
            images
                .iter()
                .all(|image| image.path.file_name().is_none_or(|name| name != "libsystem_pthread.dylib")),
            "pthread was already published at the launch stop: {images:?}"
        );
        let main = backend(&mut d).threads().expect("threads")[0];
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(main, ThreadAction::Continue))
            .expect("resume from the entry stop");
        assert!(
            !d.wait_for_stdout("new worker ran", BIRTH_QUIET),
            "a thread ran before pthread's entry was trapped, stdout: {}",
            d.stdout()
        );
        let stray = backend(&mut d).wait(Some(Duration::from_millis(50))).expect("wait");
        if let Some(event) = stray {
            panic!(
                "dyld's image notifier was delivered to the client: {}",
                describe_stray_stop(&mut d, &fixture, &event)
            );
        }
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        let newborn = backend(&mut d)
            .threads()
            .expect("threads")
            .into_iter()
            .find(|id| *id != main)
            .expect("dyld ran without creating a thread");
        let pc = pc_of(&mut d, newborn);
        assert_eq!(raw_instruction_at(&d, pc), BRK, "new thread is not held on a trap at {pc}");
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "the held thread never finished, stdout: {}",
            d.stdout()
        );
    });
}

/// An engine breakpoint on the pthread entry is reported, and the newborn does not run.
#[test]
fn breakpoint_on_pthread_entry_is_reported()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (creator, _suspended, newborn) = hold_new_thread(&mut d, &fixture);
        let pc = pc_of(&mut d, newborn);
        backend(&mut d)
            .write_memory(pc, &TRAP)
            .expect("mark the entry as an engine breakpoint");
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
            .expect("resume creator");
        let event = backend(&mut d)
            .wait(Some(STOP_TIMEOUT))
            .expect("wait")
            .expect("the entry breakpoint");
        assert!(
            matches!(event, StopEvent::Breakpoint { pc: hit, .. } if hit == pc),
            "expected a breakpoint at {pc}, got {event:?}"
        );
        assert!(
            !d.stdout().contains("second worker ran"),
            "the breakpoint thread ran: {}",
            d.stdout()
        );
        let StopEvent::Breakpoint { thread: second, .. } = event else {
            unreachable!("matched above")
        };
        assert_ne!(second, newborn, "the held newborn ran into its own entry again");
        // Answer the reported stop: that newborn steps off its entry and runs.
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(second, ThreadAction::Continue))
            .expect("continue the reported newborn");
        assert!(
            d.wait_for_stdout("second worker ran", STOP_TIMEOUT),
            "the reported newborn did not run, stdout: {}",
            d.stdout()
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        let name = u32::try_from(second.raw()).expect("thread port name");
        assert!(
            diagnostics::send_right_count(name).expect("refs") >= 1,
            "the backend released its own send right to the reported newborn"
        );
        backend(&mut d)
            .read_registers(second)
            .expect("registers of the reported newborn");
    });
}

/// A software breakpoint on `_thread_start` survives the internal step-over that arms the
/// birth traps and the continue that removes them.
#[test]
fn session_breakpoint_on_thread_start_survives_a_step_over()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let spawn_later = function_address(&mut d, &fixture, "spawn_later");
        d.add_breakpoint(BreakpointRequest::Software { address: spawn_later })
            .expect("break on spawn_later");
        d.resume().expect("run to spawn_later");
        let creator = match d.wait(Some(STOP_TIMEOUT)).expect("wait") {
            Some(StopEvent::Breakpoint { thread, pc }) if pc == spawn_later => thread,
            other => panic!("expected the spawn_later breakpoint, got {other:?}"),
        };
        let worker = d
            .threads()
            .expect("threads")
            .into_iter()
            .find(|id| *id != creator)
            .expect("the first worker");
        let entry = thread_start_of(&mut d, worker);
        d.add_breakpoint(BreakpointRequest::Software { address: entry })
            .expect("break on _thread_start");
        // Leaving spawn_later steps over its trap with every other thread suspended,
        // which arms the birth traps, and then continues every thread.
        d.resume().expect("resume from spawn_later");
        match d.wait(Some(STOP_TIMEOUT)).expect("wait") {
            Some(StopEvent::Breakpoint { pc, .. }) if pc == entry => {}
            other => panic!(
                "the _thread_start breakpoint did not stop the new thread: {other:?}, stdout: {}",
                d.stdout()
            ),
        }
        assert!(
            !d.stdout().contains("new worker ran"),
            "the new thread ran past its breakpoint"
        );
    });
}

/// `read_memory` shows the original instruction under an armed birth trap, so a breakpoint
/// inserted and removed there puts that instruction back.
#[test]
fn armed_birth_trap_is_hidden_from_reads_and_breakpoints()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (_creator, _suspended, newborn) = hold_new_thread(&mut d, &fixture);
        let pc = pc_of(&mut d, newborn);
        assert_eq!(
            instruction_at(&mut d, pc),
            STP_XZR_XZR_PREDEC,
            "read_memory showed the backend's own trap"
        );
        let id = d
            .add_breakpoint(BreakpointRequest::Software { address: pc })
            .expect("break on the held entry");
        d.remove_breakpoint(id).expect("remove the breakpoint");
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "removing the breakpoint left a trap on the entry, stdout: {}",
            d.stdout()
        );
    });
}

/// Where a stop nobody expected was raised: its image, whether it is the executable entry
/// (the birth policy's internal trap), and what memory holds there now. A `brk` stop where
/// memory no longer has one was a stale instruction fetch (issue #80).
fn describe_stray_stop(d: &mut Debuggee, fixture: &Fixture, event: &StopEvent) -> String
{
    let StopEvent::Breakpoint { pc, .. } = event else {
        return format!("{event:?}");
    };
    let entry = executable_entry(d, fixture);
    let image = backend(d)
        .images()
        .ok()
        .and_then(|images| {
            images
                .into_iter()
                .filter(|image| image.load_address <= *pc)
                .max_by_key(|image| image.load_address)
        })
        .map_or_else(|| "no image".to_owned(), |image| image.path.display().to_string());
    let live = diagnostics::raw_memory(d.pid(), *pc, 4).map_or_else(|err| err.to_string(), |bytes| format!("{bytes:02x?}"));
    // Built with `write!`: one `format!` with every argument trips `large_stack_arrays`.
    let mut description = format!("{event:?} in {image}");
    let _ = write!(description, ", executable entry {entry} (match: {})", *pc == entry);
    let _ = write!(description, ", memory there now {live}");
    description
}

/// The executable entry (`LC_MAIN`) of `fixture`, in the running process.
fn executable_entry(d: &mut Debuggee, fixture: &Fixture) -> Address
{
    let bytes = std::fs::read(fixture.path()).expect("read the fixture");
    let file = object::File::parse(&*bytes).expect("parse the fixture");
    let entryoff = object::Object::entry(&file);
    let image = backend(d)
        .images()
        .expect("images")
        .into_iter()
        .find(|image| image.path == fixture.path())
        .expect("the main executable image");
    Address::from(image.load_address.value() + entryoff)
}

/// A breakpoint inserted on the executable entry while the backend's own trap is there is
/// reported, and new threads are still held after it.
#[test]
fn breakpoint_on_main_is_reported_while_the_entry_trap_is_armed()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let entry = executable_entry(&mut d, &fixture);
        let main = backend(&mut d).threads().expect("threads")[0];
        // A step from the launch stop arms the executable entry trap, which stays in
        // memory while the process is stopped.
        assert_eq!(step(&mut d, main), StopEvent::SingleStep { thread: main });
        assert_eq!(raw_instruction_at(&d, entry), BRK, "the step did not arm the entry trap");
        let id = d
            .add_breakpoint(BreakpointRequest::Software { address: entry })
            .expect("break on the executable entry");
        let from_launch = ResumeRequest::all(ThreadAction::Suspend).with_thread(main, ThreadAction::Continue);
        backend(&mut d).resume(&from_launch).expect("resume from the launch stop");
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait"),
            Some(StopEvent::Breakpoint { thread: main, pc: entry }),
            "the executable entry trap swallowed the breakpoint on it"
        );
        d.remove_breakpoint(id).expect("remove the entry breakpoint");
        backend(&mut d).resume(&from_launch).expect("resume main");
        assert!(
            !d.wait_for_stdout("new worker ran", BIRTH_QUIET),
            "a thread ran after the entry breakpoint, stdout: {}",
            d.stdout()
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "the process did not finish, stdout: {}",
            d.stdout()
        );
    });
}

/// A step from the launch stop installs the executable entry trap. The continue after it
/// removes that trap, so it cannot arm pthread entries and hold threads later.
#[test]
fn continue_after_a_step_from_launch_holds_no_thread()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let main = backend(&mut d).threads().expect("threads")[0];
        assert_eq!(step(&mut d, main), StopEvent::SingleStep { thread: main });
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "a continue of every thread held a thread, stdout: {}",
            d.stdout()
        );
    });
}

/// A thread that existed before the resume and was told to `Continue` runs, even when
/// it re-enters `_start_wqthread` as a reused workqueue thread.
#[test]
fn reused_workqueue_thread_with_explicit_continue_runs()
{
    harness::run(|t| {
        let fixture = fixture!("workqueue");
        let mut d = t.launch(&fixture);
        let address = function_address(&mut d, &fixture, "submit_second");
        let original = plant_trap(&mut d, address);
        // Only the main thread calls submit_second, once, so no peer can reach this trap.
        let (creator, _) = run_to_trap(&mut d, address);
        backend(&mut d).write_memory(address, &original).expect("restore");
        let mut request = ResumeRequest::all(ThreadAction::Suspend);
        for id in backend(&mut d).threads().expect("threads") {
            request = request.with_thread(id, ThreadAction::Continue);
        }
        assert_ne!(request.action_for(creator), ThreadAction::Suspend);
        backend(&mut d).resume(&request).expect("continue every existing thread");
        assert!(
            d.wait_for_stdout("second block ran", STOP_TIMEOUT),
            "an existing thread with Continue was held, stdout: {}",
            d.stdout()
        );
        assert!(
            d.stdout().contains("second block ran, same thread: true"),
            "the second work item did not reuse the first thread, so this test proves nothing: {}",
            d.stdout()
        );
        assert!(
            d.wait_for_stdout("workqueue finished", STOP_TIMEOUT),
            "the fixture did not finish, stdout: {}",
            d.stdout()
        );
    });
}

/// A requested step of the held entry keeps the creator from starting another thread.
#[test]
fn user_step_of_birth_entry_keeps_the_creator_stopped()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (creator, _suspended, newborn) = stop_before_second_spawn(&mut d, &fixture);
        let creator_pc = pc_of(&mut d, creator);
        backend(&mut d)
            .resume(
                &ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(creator, ThreadAction::Continue)
                    .with_thread(newborn, ThreadAction::Step),
            )
            .expect("step the newborn and continue the creator");
        let event = backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait").expect("step stop");
        assert_eq!(event, StopEvent::SingleStep { thread: newborn });
        assert!(
            !d.stdout().contains("second worker ran"),
            "creator ran during the step: {}",
            d.stdout()
        );
        assert_eq!(pc_of(&mut d, creator), creator_pc, "the creator moved during the step");
    });
}

/// A failed suspend of the newborn leaves it stopped and does not run its body.
#[test]
fn failed_birth_suspend_does_not_let_the_new_thread_run()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[], debugger);
        let (creator, _suspended) = stop_before_second_thread(&mut d, &fixture);
        faults.fail_next_birth_suspend();
        backend(&mut d)
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
            .expect("resume creator");
        assert!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).is_err(),
            "the failed thread_suspend was not reported"
        );
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        assert!(!d.stdout().contains("new worker ran"), "the newborn ran: {}", d.stdout());
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "the process stayed stuck after the failed suspend, stdout: {}",
            d.stdout()
        );
    });
}

/// A resume that fails after the birth traps are installed leaves the target stopped and runnable.
#[test]
fn failed_birth_resume_can_still_continue()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[], debugger);
        let (creator, _suspended) = stop_before_second_thread(&mut d, &fixture);
        faults.fail_next_task_resume();
        let failed =
            backend(&mut d).resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue));
        assert!(failed.is_err(), "the injected task_resume failure was ignored: {failed:?}");
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("continue after the failure");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "the target stayed stuck after a failed resume, stdout: {}",
            d.stdout()
        );
    });
}

/// Detach, while a newborn is held, lets the process finish. Drop of a launched target
/// that still holds one is covered by killing below.
#[test]
fn detach_releases_a_held_new_thread_and_the_process_finishes()
{
    harness::run_with_timeout(Duration::from_secs(30), |t| {
        let fixture = fixture!("spawn_thread");
        let mut target = t.spawn(&fixture, &["pause=4000"]);
        let pid = target.pid();
        let mut session = ferros_macos::new_session().expect("backend");
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match session.attach(pid) {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(err) => panic!("attach failed: {err}"),
            }
        }
        let address = function_address(&mut session, &fixture, "spawn_later");
        let original = {
            let bytes = session.read_memory(address, TRAP.len()).expect("read");
            session.write_memory(address, &TRAP).expect("trap");
            bytes
        };
        session.resume().expect("run to spawn_later");
        let creator = match session.wait(Some(STOP_TIMEOUT)).expect("wait") {
            Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
            other => panic!("expected the spawn_later breakpoint, got {other:?}"),
        };
        session.write_memory(address, &original).expect("restore");
        let threads = session.threads().expect("threads");
        let suspended: Vec<_> = threads
            .into_iter()
            .filter(|&id| id != creator)
            .map(|id| (id, session.read_registers_for(id).expect("registers").pc))
            .collect();
        session
            .backend_mut()
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
            .expect("hold new threads");
        std::thread::sleep(BIRTH_QUIET);
        session.backend_mut().interrupt().expect("interrupt the hold");
        assert!(session.wait(Some(STOP_TIMEOUT)).expect("stop").is_some());
        for (id, pc) in suspended {
            assert_eq!(
                session.read_registers_for(id).expect("registers").pc,
                pc,
                "suspended thread moved"
            );
        }
        session.detach().expect("detach");
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if let Some(status) = target.try_wait().expect("status") {
                assert!(status.success(), "detached process failed: {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "detached process did not finish: {}",
                process_state(pid.0)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

/// Dropping an attached session while a newborn is held detaches and lets it finish.
#[test]
fn drop_releases_a_held_new_thread_and_the_process_finishes()
{
    harness::run_with_timeout(Duration::from_secs(30), |t| {
        let fixture = fixture!("spawn_thread");
        let mut target = t.spawn(&fixture, &["pause=4000"]);
        let pid = target.pid();
        let mut session = ferros_macos::new_session().expect("backend");
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match session.attach(pid) {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(err) => panic!("attach failed: {err}"),
            }
        }
        let address = function_address(&mut session, &fixture, "spawn_later");
        let original = {
            let bytes = session.read_memory(address, TRAP.len()).expect("read");
            session.write_memory(address, &TRAP).expect("trap");
            bytes
        };
        session.resume().expect("run to spawn_later");
        let creator = match session.wait(Some(STOP_TIMEOUT)).expect("wait") {
            Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
            other => panic!("expected the spawn_later breakpoint, got {other:?}"),
        };
        session.write_memory(address, &original).expect("restore");
        session
            .backend_mut()
            .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
            .expect("hold new threads");
        std::thread::sleep(BIRTH_QUIET);
        session.backend_mut().interrupt().expect("interrupt the hold");
        assert!(session.wait(Some(STOP_TIMEOUT)).expect("stop").is_some());
        drop(session);
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if let Some(status) = target.try_wait().expect("status") {
                assert!(status.success(), "dropped session left a failing process: {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "dropped session did not let the process finish: {}",
                process_state(pid.0)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

/// Killing a launched process that holds a newborn releases that hold and reaps it.
#[test]
fn kill_releases_a_held_new_thread()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let pid = d.pid();
        let _held = hold_new_thread(&mut d, &fixture);
        backend(&mut d).kill().expect("kill");
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !process_state(pid.0).is_empty() {
            assert!(
                Instant::now() < deadline,
                "killed process still present: {}",
                process_state(pid.0)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}
