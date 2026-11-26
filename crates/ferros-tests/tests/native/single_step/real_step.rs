//! Real single-step races: interrupts, signals and publication pauses.

use std::time::{Duration, Instant};

use ferros_core::{Address, Backend, BreakpointRequest, ResumeRequest, StopEvent, ThreadAction, ThreadId};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::arm64::{Flow, flow};
use ferros_tests::control::ControlDir;
use ferros_tests::symbols::{function_address, marker_address};
use ferros_tests::{fixture, harness};

use super::helpers::*;

/// Disarm or release the publication hook even if an assertion unwinds, so a hook
/// armed but not yet reached cannot pause a late exception during teardown (#183).
struct ReleasePublication(ferros_macos::exception::ReplyFaultInjector);
impl Drop for ReleasePublication
{
    fn drop(&mut self)
    {
        self.0.cancel_stop_publication();
    }
}

fn wait_for_publication_pause(faults: &ferros_macos::exception::ReplyFaultInjector)
{
    let deadline = Instant::now() + Duration::from_secs(3);
    while !faults.stop_publication_is_paused() {
        assert!(Instant::now() < deadline, "real exception never reached publication pause");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_published_stop(faults: &ferros_macos::exception::ReplyFaultInjector)
{
    let deadline = Instant::now() + Duration::from_secs(3);
    while !faults.has_pending_stop() {
        assert!(Instant::now() < deadline, "released real exception was never published");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn assert_step_cleaned(stepped: ThreadId, threads: &[ThreadId])
{
    assert!(
        !diagnostics::single_step_enabled(stepped).expect("MDSCR_EL1.SS"),
        "single-step bit still set immediately after cancellation"
    );
    for &thread in threads {
        assert_eq!(
            diagnostics::thread_suspend_count(thread).expect("suspend count"),
            0,
            "thread {thread:?} still suspended immediately after cancellation"
        );
    }
}

/// Both detach paths start with directly observed per-thread suspensions. The
/// publication pause holds the real step completion until cleanup is deliberately begun.
#[test]
fn detach_cleanup_after_starting_step_releases_suspended_threads()
{
    harness::run_with_timeout(Duration::from_secs(40), |t| {
        for explicit in [false, true] {
            let control = ControlDir::new();
            let fixture = fixture!("step_races");
            let mut target = t.spawn(&fixture, &[control.path().to_str().expect("path")]);
            control.wait("ready-0");
            control.wait("ready-1");
            let debugger = MacOSBackend::new().expect("backend");
            let faults = debugger.reply_fault_injector();
            let mut session = ferros_macos::session_with(debugger);
            let _release = ReleasePublication(faults.clone());
            session.attach(target.pid()).expect("attach");
            let address = marker_address(&mut session, &fixture, "step_site");
            let id = session
                .add_breakpoint(BreakpointRequest::Software { address })
                .expect("breakpoint");
            control.release("run-0");
            session.resume().expect("resume");
            let stepped = match session.wait(Some(STOP_TIMEOUT)).expect("breakpoint") {
                Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
                other => panic!("expected first worker breakpoint: {other:?}"),
            };
            session.remove_breakpoint(id).expect("remove breakpoint");
            let threads = session.threads().expect("threads");
            faults.pause_next_stop_publication();
            session.step_instruction(stepped).expect("start step");
            wait_for_publication_pause(&faults);
            for &thread in threads.iter().filter(|&&thread| thread != stepped) {
                assert_eq!(
                    diagnostics::thread_suspend_count(thread).expect("suspend count"),
                    1,
                    "other thread was not suspended while step pending"
                );
            }
            if explicit {
                session.interrupt().expect("interrupt step");
                assert_eq!(
                    session.wait(Some(STOP_TIMEOUT)).expect("interrupt"),
                    Some(StopEvent::Interrupted { thread: None })
                );
                assert_step_cleaned(stepped, &threads);
            }
            if !explicit {
                // The public contract stays stopped-only while the step is pending.
                assert!(session.detach().is_err(), "detach was allowed while running");
            }
            // Publish the late real completion before either release path, so each run
            // takes one ordering: drop's own interrupt is consumed by that pending stop
            // and its wait receives the SingleStep; the explicit path drains it first.
            faults.release_stop_publication();
            wait_for_published_stop(&faults);
            if explicit {
                assert_eq!(
                    session.wait(Some(STOP_TIMEOUT)).expect("late step"),
                    Some(StopEvent::SingleStep { thread: stepped })
                );
                session.detach().expect("detach after stop");
            }
            drop(session);
            control.release("run-1");
            control.release("exit");
            let deadline = Instant::now() + STOP_TIMEOUT;
            loop {
                if let Some(status) = target.try_wait().expect("child status") {
                    assert!(status.success(), "detached process failed (explicit: {explicit}): {status}");
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "attached process did not finish (explicit: {explicit})"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
}

/// The kernel has already raised `SingleStep`, but publication is held until an
/// interrupt is queued. This forces the interrupt to win every time.
#[test]
fn interrupt_precedes_a_real_instruction_step()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], debugger);
        let tick = function_address(&mut d, &fixture, "worker_body");
        let original = plant_trap(&mut d, tick);
        // The other worker stops at the trap too and holds that stop through the step.
        let (thread, _) = run_to_both_workers(&mut d, tick);
        backend(&mut d).write_memory(tick, &original).expect("restore");
        let threads = backend(&mut d).threads().expect("threads");
        let _release = ReleasePublication(faults.clone());
        faults.pause_next_stop_publication();
        backend(&mut d).resume(&ResumeRequest::step(thread)).expect("step");
        wait_for_publication_pause(&faults);
        backend(&mut d).interrupt().expect("interrupt");
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait"),
            Some(StopEvent::Interrupted { thread: None })
        );
        assert_step_cleaned(thread, &threads);
        faults.release_stop_publication();
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("late step"),
            Some(StopEvent::SingleStep { thread })
        );
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(100))).expect("no stray step"),
            None
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
    });
}

/// A planted brk is synchronously taken before the instruction can complete. Unlike
/// an externally sent signal, its ordering against single-step is architectural.
#[test]
fn breakpoint_cancels_a_real_step_without_a_stray_step_stop()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "worker_body");
        let original = plant_trap(&mut d, tick);
        // The other worker stops at the trap too, so its hit cannot replace the step's stop.
        let (thread, _) = run_to_both_workers(&mut d, tick);
        let threads = backend(&mut d).threads().expect("threads");
        // Leave the real brk at pc: the step executes it instead of the original.
        assert_eq!(step(&mut d, thread), StopEvent::Breakpoint { thread, pc: tick });
        assert_step_cleaned(thread, &threads);
        backend(&mut d).write_memory(tick, &original).expect("restore");
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert_eq!(
            backend(&mut d).wait(Some(Duration::from_millis(100))).expect("no stray step"),
            None
        );
        backend(&mut d).interrupt().expect("interrupt");
        assert!(matches!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
    });
}

/// An empty socket keeps the stepping thread in its first instruction's syscall
/// until the kernel can report SIGUSR1 on that same thread.
#[cfg(target_arch = "aarch64")]
#[test]
fn signal_cancels_a_real_step_without_a_stray_step_stop()
{
    harness::run(|t| {
        let control = ControlDir::new();
        let fixture = fixture!("step_races");
        let mut d = t.launch_with_args(&fixture, &[control.path().to_str().expect("path"), "--block-in-read"]);
        let read = ferros_tests::symbols::raw_symbol_address(&mut d, &fixture, "_ferros_step_fixture_read");
        // The two setup instructions load the one-byte length and Darwin syscall number.
        let svc = Address::from(read.value() + 2 * 4);
        let original = plant_trap(&mut d, svc);
        assert_eq!(original, 0xd400_1001_u32.to_le_bytes(), "expected svc #0x80");
        control.release("run-0");
        // Only worker 0 runs to the trap: worker 1 waits for `run-1`, released only after
        // the syscall is restored, and the main thread never calls the read helper.
        let (stepped, others) = run_to_trap(&mut d, svc);
        assert!(others.is_empty(), "only worker 0 was released");
        backend(&mut d).write_memory(svc, &original).expect("restore the syscall");
        let threads = backend(&mut d).threads().expect("threads");
        assert!(threads.len() >= 3, "expected main and both workers");

        backend(&mut d).resume(&ResumeRequest::step(stepped)).expect("start step");
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !diagnostics::thread_is_waiting(stepped).expect("thread run state") {
            assert!(Instant::now() < deadline, "stepping thread never waited in the kernel");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(diagnostics::single_step_enabled(stepped).expect("armed MDSCR_EL1.SS"));
        for &peer in threads.iter().filter(|&&thread| thread != stepped) {
            assert_eq!(diagnostics::thread_suspend_count(peer).expect("peer suspend count"), 1);
        }
        diagnostics::send_sigusr1(stepped).expect("send SIGUSR1 to the stepping thread");
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("signal stop"),
            Some(StopEvent::Signal {
                thread: stepped,
                signal: SIGUSR1,
            })
        );
        assert_step_cleaned(stepped, &threads);
        // The signal interrupted the blocking `svc`: the thread is still at the syscall
        // (restarted) or just after it, never beyond, so the step did not complete.
        let pc = backend(&mut d).read_registers(stepped).expect("registers").pc;
        assert!(
            pc == svc || pc == Address::from(svc.value() + 4),
            "signal stop at {pc:?}, expected the svc at {svc:?} or the instruction after it"
        );

        control.release("run-1");
        control.release("exit");
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("continue");
        assert_eq!(
            backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("exit after continue"),
            Some(StopEvent::Exited { code: 0 }),
            "continuing after the signal must finish without a stray step"
        );
    });
}

/// A real internal step completion arriving after abandonment belongs to no user
/// request. Waiting after Interrupted must consume it silently, without a resume.
#[test]
fn late_real_internal_step_is_dropped_after_abandonment()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[LONG_LOOP], debugger);
        let address = function_address(&mut d, &fixture, "tick");
        let id = d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
        d.resume().expect("resume");
        assert!(
            matches!(d.wait(Some(STOP_TIMEOUT)).expect("breakpoint"), Some(StopEvent::Breakpoint { pc, .. }) if pc == address)
        );
        let _release = ReleasePublication(faults.clone());
        faults.pause_next_stop_publication();
        d.resume().expect("internal step times out waiting for publication");
        wait_for_publication_pause(&faults);
        d.interrupt().expect("interrupt");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("interrupt"),
            Some(StopEvent::Interrupted { thread: None })
        );
        faults.release_stop_publication();
        wait_for_published_stop(&faults);
        assert_eq!(
            d.wait(Some(Duration::from_millis(100))).expect("drain late real step"),
            None,
            "abandoned internal SingleStep reached the caller"
        );
        d.remove_breakpoint(id).expect("remove breakpoint");
        d.resume().expect("continue");
        assert_eq!(d.wait(Some(Duration::from_millis(100))).expect("running"), None);
        d.interrupt().expect("interrupt");
        assert!(matches!(
            d.wait(Some(STOP_TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
    });
}

/// Complete a real internal step with a real signal in the same exception batch.
/// Session must return that signal before issuing the final continue.
#[test]
fn event_batched_with_real_internal_step_is_reported_before_resume()
{
    harness::run(|t| {
        let control = ControlDir::new();
        let fixture = fixture!("step_races");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[control.path().to_str().expect("path")], debugger);
        let address = marker_address(&mut d, &fixture, "step_site");
        let id = d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
        control.release("run-0");
        d.resume().expect("run first worker");
        let stepped = match d.wait(Some(STOP_TIMEOUT)).expect("breakpoint") {
            Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
            other => panic!("expected worker breakpoint: {other:?}"),
        };
        let threads = d.session_mut().threads().expect("threads");
        // Identify the workers by the system-wide ids the fixture reported, not by
        // the order of the thread list.
        control.wait("ready-1");
        let worker = |slot: usize| control.read(&format!("tid-{slot}")).parse::<u64>().expect("worker id");
        assert_eq!(
            diagnostics::thread_unique_id(stepped).expect("stepped id"),
            worker(0),
            "the breakpoint stop was not worker 0"
        );
        let peer = *threads
            .iter()
            .find(|&&id| diagnostics::thread_unique_id(id).expect("thread id") == worker(1))
            .expect("second worker");
        let instruction = u32::from_le_bytes(
            d.read_memory(address, 4)
                .expect("original instruction")
                .try_into()
                .expect("instruction"),
        );
        let expected_pc = match flow(address.value(), instruction) {
            Flow::Next => address.value() + 4,
            Flow::Direct(target) => target,
            other => panic!("step_site marker needs a fixed successor, got {other:?}"),
        };
        faults.batch_next_step_with_signal(peer);
        d.resume().expect("finish internal step without resuming over pending signal");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("batched signal"),
            Some(StopEvent::Signal {
                thread: peer,
                signal: SIGUSR1
            })
        );
        assert_eq!(
            pc_of(&mut d, stepped).value(),
            expected_pc,
            "internal step resumed past its completion while another event was pending"
        );
        // The hook balanced and restored exactly one peer suspension, and delivery then
        // released the step request's suspensions: nothing per-thread may remain.
        for &thread in &threads {
            assert_eq!(
                diagnostics::thread_suspend_count(thread).expect("suspend count"),
                0,
                "thread {thread:?} kept a suspension after the batched stop"
            );
        }
        let pcs: Vec<_> = threads.iter().map(|&id| (id, pc_of(&mut d, id))).collect();
        assert_eq!(d.wait(Some(Duration::from_millis(50))).expect("no other event"), None);
        for (id, pc) in pcs {
            assert_eq!(pc_of(&mut d, id), pc, "thread ran while pending event was reported");
        }
        assert!(!control.path().join("done-0").exists(), "worker resumed past step_site");
        d.remove_breakpoint(id).expect("remove breakpoint");
    });
}

/// Retain worker 1's signal while worker 0 leaves a breakpoint, then abandon that
/// internal step. Only `resume_with_signal` may later deliver worker 1's held signal.
#[test]
fn retained_signal_survives_abandoned_real_step_over_or_is_explicitly_dropped()
{
    harness::run_with_timeout(Duration::from_secs(40), |t| {
        for deliver in [true, false] {
            let control = ControlDir::new();
            let fixture = fixture!("step_races");
            let debugger = MacOSBackend::new().expect("backend");
            let faults = debugger.reply_fault_injector();
            let mut d = t.launch_with_backend(&fixture, &[control.path().to_str().expect("path")], debugger);
            let address = marker_address(&mut d, &fixture, "step_site");
            let id = d.add_breakpoint(BreakpointRequest::Software { address }).expect("breakpoint");
            control.release("run-0");
            d.resume().expect("run worker 0");
            let stepped = match d.wait(Some(STOP_TIMEOUT)).expect("breakpoint") {
                Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
                other => panic!("expected worker 0 breakpoint: {other:?}"),
            };
            // Restore only while admitting worker 1: it must pass step_site and
            // raise its own SIGUSR1 while worker 0's breakpoint record stays held.
            d.disable_breakpoint(id).expect("disable trap for worker 1");
            control.release("signal-1");
            control.release("run-1");
            backend(&mut d)
                .resume(&ResumeRequest::continue_all().with_thread(stepped, ThreadAction::Suspend))
                .expect("run signalled worker");
            let signalled = match d.wait(Some(STOP_TIMEOUT)).expect("worker signal") {
                Some(StopEvent::Signal { thread, signal: SIGUSR1 }) => thread,
                other => panic!("expected worker 1 SIGUSR1: {other:?}"),
            };
            assert_ne!(signalled, stepped);
            d.enable_breakpoint(id).expect("restore trap for internal step");
            let _release = ReleasePublication(faults.clone());
            faults.pause_next_stop_publication();
            d.resume_with_signal().expect("start step-over with retained signal for peer");
            wait_for_publication_pause(&faults);
            d.interrupt().expect("abandon internal step");
            assert_eq!(
                d.wait(Some(STOP_TIMEOUT)).expect("interrupt"),
                Some(StopEvent::Interrupted { thread: None })
            );
            faults.release_stop_publication();
            wait_for_published_stop(&faults);
            assert_eq!(d.wait(Some(Duration::from_millis(100))).expect("late internal step"), None);
            d.remove_breakpoint(id).expect("remove breakpoint");
            if deliver {
                d.resume_with_signal().expect("deliver carried worker signal");
            } else {
                d.resume().expect("drop carried worker signal");
            }
            control.wait("done-1");
            control.release("snapshot");
            control.wait("observed");
            assert_eq!(
                control.read("observed"),
                if deliver { "2" } else { "0" },
                "wrong fixture-side handlers after abandoned step (deliver: {deliver})"
            );
            assert_eq!(
                d.wait(Some(Duration::from_millis(50))).expect("no stray step or signal"),
                None
            );
            d.interrupt().expect("stop");
            assert!(matches!(
                d.wait(Some(STOP_TIMEOUT)).expect("interrupt"),
                Some(StopEvent::Interrupted { .. })
            ));
            if !deliver {
                // A plain resume must forget the carried decision, not merely defer
                // it again. A later signal-passing resume must also remain silent.
                std::fs::remove_file(control.path().join("snapshot")).expect("clear snapshot request");
                std::fs::remove_file(control.path().join("observed")).expect("clear snapshot");
                d.resume_with_signal()
                    .expect("plain resume discarded carried signal permanently");
                control.release("snapshot");
                control.wait("observed");
                assert_eq!(control.read("observed"), "0", "plain resume kept a carried signal");
                d.interrupt().expect("stop after second resume");
                assert!(matches!(
                    d.wait(Some(STOP_TIMEOUT)).expect("stop"),
                    Some(StopEvent::Interrupted { .. })
                ));
            }
        }
    });
}

/// Backend drop must also undo the step without any intervening wait/deliver. This
/// covers teardown's own cleanup independently of Session's interrupt/wait on drop.
#[test]
fn backend_drop_releases_directly_observed_step_suspends()
{
    harness::run(|t| {
        let control = ControlDir::new();
        let mut target = t.spawn(&fixture!("step_races"), &[control.path().to_str().expect("path")]);
        control.wait("ready-0");
        control.wait("ready-1");
        let mut debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        debugger.attach(target.pid()).expect("attach");
        let threads = debugger.threads().expect("threads");
        assert!(threads.len() >= 3);
        let stepped = threads[1];
        debugger.resume(&ResumeRequest::step(stepped)).expect("step");
        for &thread in threads.iter().filter(|&&id| id != stepped) {
            assert_eq!(diagnostics::thread_suspend_count(thread).expect("suspend count"), 1);
        }
        // Do not call wait: even if a real completion is already queued, only the
        // backend's teardown (detach_target and release_ports) can balance the
        // suspensions on this path.
        let peer = threads[0];
        let sample = faults.observe_next_detach_suspend_count(peer);
        drop(debugger);
        assert_eq!(
            sample
                .try_recv()
                .expect("sample before detach SIGSTOP")
                .expect("peer suspend count"),
            0,
            "detach sent its private SIGSTOP while peer {peer:?} was still suspended"
        );
        control.release("run-0");
        control.release("run-1");
        control.release("exit");
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if let Some(status) = target.try_wait().expect("child status") {
                assert!(status.success(), "backend drop left a failing child: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "backend drop left worker threads suspended");
            std::thread::sleep(Duration::from_millis(10));
        }
    });
}

/// A handler's return can be published after a peer's breakpoint step starts (#176).
/// The reissued continue must consume the fresh step of that same peer and run to exit.
#[test]
fn handler_return_reissues_a_real_peer_breakpoint_step()
{
    use ferros_core::BreakpointLocation;
    use ferros_macos::guards::ThreadSuspendGuard;

    harness::run(|t| {
        let control = ControlDir::new();
        let fixture = fixture!("step_races");
        let debugger = MacOSBackend::new().expect("backend");
        let faults = debugger.reply_fault_injector();
        // Session steps breakpoint sites in the order of its thread list. Mach does not
        // promise `task_threads` order across stops (#183), so the backend reports the
        // list sorted by port name, which is stable for a live thread.
        faults.sort_thread_lists();
        // The paused return is held across an interrupt, a wait and a resume; give that
        // the same bound as every other wait here rather than the hook's default.
        faults.set_stop_publication_deadline(STOP_TIMEOUT);
        let mut d = t.launch_with_backend(&fixture, &[control.path().to_str().expect("path")], debugger);
        let interrupted = Some(StopEvent::Interrupted { thread: None });
        d.resume().expect("start gated workers");
        control.wait("ready-0");
        control.wait("ready-1");
        d.interrupt().expect("stop to enumerate workers");
        assert_eq!(d.wait(Some(STOP_TIMEOUT)).expect("startup interrupt"), interrupted);

        // Choose the earlier worker in that sorted list as the peer, because the handler
        // will also be at a breakpoint and Session must step the peer first.
        let worker_ids = ["tid-0", "tid-1"].map(|file| control.read(file).parse::<u64>().expect("worker id"));
        let workers: Vec<_> = d
            .threads()
            .expect("threads")
            .into_iter()
            .filter_map(|thread| {
                let id = diagnostics::thread_unique_id(thread).expect("thread id");
                worker_ids.iter().position(|&worker| worker == id).map(|slot| (thread, slot))
            })
            .collect();
        let &[(peer, peer_slot), (handler, handler_slot)] = workers.as_slice() else {
            panic!("expected two gated workers, got {workers:?}");
        };

        let handler_breakpoint = d
            .set_breakpoint(BreakpointLocation::Function(
                "ferros_macos::diagnostics::observe_sigusr1".into(),
            ))
            .expect("breakpoint inside the handler");
        control.release(&format!("signal-{handler_slot}"));
        control.release(&format!("run-{handler_slot}"));
        d.resume().expect("run the signal worker");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("signal stop"),
            Some(StopEvent::Signal {
                thread: handler,
                signal: SIGUSR1
            })
        );
        let saved = d.read_registers_for(handler).expect("interrupted registers");
        d.add_breakpoint(BreakpointRequest::Software { address: saved.pc })
            .expect("return trap");
        d.resume_with_signal().expect("run to the handler breakpoint");
        assert!(matches!(d.wait(Some(STOP_TIMEOUT)).expect("handler stop"),
            Some(StopEvent::Breakpoint { thread, .. }) if thread == handler));
        assert_eq!(d.breakpoint_hit().expect("handler hit").id, handler_breakpoint);
        d.delete_breakpoint(handler_breakpoint).expect("remove handler breakpoint");

        let peer_site = marker_address(&mut d, &fixture, "step_site");
        d.add_breakpoint(BreakpointRequest::Software { address: peer_site })
            .expect("peer breakpoint");
        control.release(&format!("run-{peer_slot}"));
        backend(&mut d)
            .resume(&ResumeRequest::continue_all().with_thread(handler, ThreadAction::Suspend))
            .expect("run the peer while keeping the handler stopped");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("peer stop"),
            Some(StopEvent::Breakpoint {
                thread: peer,
                pc: peer_site
            })
        );

        // Keep the peer at its original PC even after Session arms its first step.
        // The kernel takes the handler's return trap, but the hook hides it across
        // the interrupt and the next resume so that resume can accept the peer step.
        let held_peer = ThreadSuspendGuard::new(u32::try_from(peer.raw()).expect("peer port")).expect("hold peer");
        let _release = ReleasePublication(faults.clone());
        faults.pause_next_stop_publication();
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("run handler to the return trap");
        wait_for_publication_pause(&faults);
        backend(&mut d).interrupt().expect("interrupt ahead of the paused return");
        assert_eq!(d.wait(Some(STOP_TIMEOUT)).expect("racing interrupt"), interrupted);
        let returned = d.read_registers_for(handler).expect("handler return registers");
        assert_eq!((returned.pc, returned.sp), (saved.pc, saved.sp));
        assert_eq!(pc_of(&mut d, peer), peer_site);

        d.resume().expect("start the peer step while publication is paused");
        assert!(faults.stop_publication_is_paused());
        assert!(
            diagnostics::single_step_enabled(peer).expect("peer step bit"),
            "Session did not step the peer first (threads now {:?})",
            d.threads().expect("threads")
        );
        faults.release_stop_publication();
        wait_for_published_stop(&faults);
        // The return stop now holds the whole task. Removing the test suspension
        // lets the *reissued* step run after the backend cancels the first one.
        held_peer.resume().expect("release peer under the task stop");
        control.release("exit");
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("wait through both step-overs"),
            Some(StopEvent::Exited { code: 0 })
        );
        control.wait("done-0");
        control.wait("done-1");
    });
}

/// Dropping the publication guard while the hook is armed but no exception has reached
/// it disarms the hook (#183): the next real stop is published without pausing, so a
/// test that unwinds early cannot leave a late exception paused during teardown.
#[test]
fn publication_guard_disarms_an_armed_hook()
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

        // A pause would hold the stop far longer than the wait below allows.
        faults.set_stop_publication_deadline(Duration::from_secs(60));
        faults.pause_next_stop_publication();
        assert!(faults.stop_publication_is_armed());
        drop(ReleasePublication(faults.clone()));
        assert!(!faults.stop_publication_is_armed(), "the guard left the hook armed");
        // Should the step pause anyway, release it once the assertions below unwind.
        let _release = ReleasePublication(faults.clone());

        assert!(
            matches!(step(&mut d, thread), StopEvent::SingleStep { thread: stepped } if stepped == thread),
            "expected the step to be published"
        );
        assert!(!faults.stop_publication_is_paused(), "a disarmed hook paused the step");
    });
}
