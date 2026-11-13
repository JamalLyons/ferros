use super::*;

fn at_breakpoint() -> (MockBackend, Session, BreakpointId)
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).unwrap();
    session.write_registers_for(T1, &regs_at(BP)).unwrap();
    let _ = mock.take_calls();
    (mock, session, id)
}

fn step(mock: &MockBackend, thread: ThreadId, pc: u64, request: ResumeRequest)
{
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread })
            .set_pc(thread, Address::new(pc))
            .expect_resume(request),
    );
}

fn hit(mock: &MockBackend, pc: u64)
{
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(pc),
        })
        .set_pc(T1, Address::new(pc))
        .expect_resume(ResumeRequest::continue_all()),
    );
}

#[test]
fn step_over_is_hidden_and_hits_are_only_real_events()
{
    let (mock, mut session, id) = at_breakpoint();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    hit(&mock, BP);
    session.resume().unwrap();
    // `resume` finished the step over and continued: the trap is back already.
    assert_eq!(peek(&mock, BP), TRAP);
    assert!(session.step_plan.is_none());
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP)
        })
    );
    assert_eq!(peek(&mock, BP), TRAP);
    assert_eq!(session.breakpoint_info(id).unwrap().hit_count, 1);
    let calls: Vec<_> = mock
        .calls()
        .into_iter()
        .filter(|call| matches!(call, MockCall::WriteMemory { .. } | MockCall::Resume(_)))
        .collect();
    assert_eq!(
        calls,
        vec![
            MockCall::WriteMemory {
                address: Address::new(BP),
                data: original(BP)
            },
            MockCall::Resume(ResumeRequest::step(T1)),
            MockCall::WriteMemory {
                address: Address::new(BP),
                data: TRAP.to_vec()
            },
            MockCall::Resume(ResumeRequest::continue_all()),
        ]
    );
}

#[test]
fn two_threads_step_over_serially_in_thread_order()
{
    let (mock, mut session, _) = at_breakpoint();
    session.add_breakpoint(software(BP2)).unwrap();
    session.write_registers_for(T2, &regs_at(BP2)).unwrap();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    step(&mock, T2, BP2 + 4, ResumeRequest::step(T2));
    hit(&mock, BP);
    session.resume().unwrap();
    assert_eq!(peek(&mock, BP2), TRAP);
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
    assert_eq!(peek(&mock, BP), TRAP);
    assert_eq!(peek(&mock, BP2), TRAP);
}

#[test]
fn consecutive_breakpoints_hit_in_order_on_each_pass()
{
    let (mock, mut session, a) = at_breakpoint();
    let b = session.add_breakpoint(software(BP + 4)).unwrap();
    for _ in 0..2 {
        step(&mock, T1, BP + 4, ResumeRequest::step(T1));
        hit(&mock, BP + 4);
        step(&mock, T1, BP + 8, ResumeRequest::step(T1));
        hit(&mock, BP);
    }
    for _ in 0..2 {
        session.resume().unwrap();
        assert_eq!(
            session.wait(None).unwrap(),
            Some(StopEvent::Breakpoint {
                thread: T1,
                pc: Address::new(BP + 4)
            })
        );
        session.resume().unwrap();
        assert_eq!(
            session.wait(None).unwrap(),
            Some(StopEvent::Breakpoint {
                thread: T1,
                pc: Address::new(BP)
            })
        );
    }
    assert_eq!(session.breakpoint_info(a).unwrap().hit_count, 2);
    assert_eq!(session.breakpoint_info(b).unwrap().hit_count, 2);
}

#[test]
fn disabled_and_removed_breakpoints_are_not_stepped_over()
{
    for remove in [false, true] {
        let (mock, mut session, id) = at_breakpoint();
        if remove {
            session.remove_breakpoint(id).unwrap();
        } else {
            session.disable_breakpoint(id).unwrap();
        }
        hit(&mock, BP2);
        session.resume().unwrap();
        session.wait(None).unwrap();
        assert!(
            !mock
                .calls()
                .iter()
                .any(|call| *call == MockCall::Resume(ResumeRequest::step(T1)))
        );
    }
}

#[test]
fn instruction_step_reports_completion_at_breakpoint_and_plain_pc()
{
    for breakpoint in [false, true] {
        let (mock, mut session, id) = at_breakpoint();
        if !breakpoint {
            session.remove_breakpoint(id).unwrap();
        }
        step(&mock, T1, BP + 4, ResumeRequest::step(T1));
        session.step_instruction(T1).unwrap();
        assert_eq!(peek(&mock, BP), original(BP));
        assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: T1 }));
        assert_eq!(peek(&mock, BP), if breakpoint { TRAP.to_vec() } else { original(BP) });
        assert_eq!(session.selected_thread(), Some(T1));
    }
}

#[test]
fn step_checks_state_and_thread_and_defers_pending_events()
{
    let mock = process();
    let mut session = session(&mock);
    assert!(matches!(session.step_instruction(T1), Err(DebuggerError::NotAttached)));
    session.launch(&spec()).unwrap();
    assert!(matches!(
        session.step_instruction(ThreadId(99)),
        Err(DebuggerError::ThreadNotFound(99))
    ));
    let event = StopEvent::Signal { thread: T2, signal: 11 };
    mock.script_simultaneous([
        MockStop::new(StopEvent::SingleStep { thread: T1 }).expect_resume(ResumeRequest::step(T1)),
        MockStop::new(event.clone()).expect_resume(ResumeRequest::step(T1)),
    ]);
    session.step_instruction(T1).unwrap();
    assert!(matches!(session.step_instruction(T1), Err(DebuggerError::NotStopped)));
    session.wait(None).unwrap();
    let _ = mock.take_calls();
    session.step_instruction(T1).unwrap();
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.wait(None).unwrap(), Some(event));
    assert!(!mock.calls().iter().any(|call| matches!(call, MockCall::Resume(_))));
}

#[test]
fn unexpected_step_events_abort_plan_and_restore_trap()
{
    for event in [
        StopEvent::Signal { thread: T1, signal: 11 },
        StopEvent::Interrupted { thread: None },
        StopEvent::Other {
            thread: Some(T2),
            reason: "other".into(),
        },
        StopEvent::SingleStep { thread: T2 },
    ] {
        let (mock, mut session, _) = at_breakpoint();
        mock.script(MockStop::new(event.clone()).expect_resume(ResumeRequest::step(T1)));
        session.resume().unwrap();
        assert_eq!(session.wait(None).unwrap(), Some(event));
        assert_eq!(peek(&mock, BP), TRAP);
        assert!(session.step_plan.is_none());
        assert!(!mock.calls().contains(&MockCall::Resume(ResumeRequest::continue_all())));
    }
}

#[test]
fn terminal_event_and_kill_discard_active_plan()
{
    for kill in [false, true] {
        let (mock, mut session, _) = at_breakpoint();
        mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::step(T1)));
        session.resume().unwrap();
        let _ = mock.take_calls();
        if kill {
            session.kill().unwrap();
        } else {
            session.wait(None).unwrap();
        }
        assert_eq!(session.state(), SessionState::Exited);
        assert!(session.step_plan.is_none());
        assert!(!mock.calls().iter().any(|call| matches!(call, MockCall::WriteMemory { .. })));
    }
}

#[test]
fn interrupt_aborts_step_over()
{
    let (mock, mut session, _) = at_breakpoint();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    session.resume().unwrap();
    session.interrupt().unwrap();
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::Interrupted { thread: None }));
    assert_eq!(peek(&mock, BP), TRAP);
    assert!(!session.interrupt_requested);
}

#[test]
fn interrupt_losing_to_single_step_reports_interrupted()
{
    let (mock, mut session, _) = at_breakpoint();
    // Nothing scripted yet: the step does not complete within `resume`, so the plan stays.
    session.resume().unwrap();
    assert!(session.step_plan.is_some());
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    // MockBackend always lets interrupt win. Set the policy flag to exercise the
    // other contract-permitted outcome without changing the backend implementation.
    session.interrupt_requested = true;
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::Interrupted { thread: Some(T1) }));
    assert_eq!(peek(&mock, BP), TRAP);
    assert!(!mock.calls().contains(&MockCall::Resume(ResumeRequest::continue_all())));
}

#[test]
fn timeout_preserves_plan_and_next_wait_finishes()
{
    let (mock, mut session, _) = at_breakpoint();
    session.resume().unwrap();
    assert_eq!(session.wait(Some(Duration::ZERO)).unwrap(), None);
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.step_plan.is_some());
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    hit(&mock, BP2);
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
}

#[test]
fn passed_signal_moves_only_to_its_own_step()
{
    for thread in [T1, T2] {
        let (mock, mut session, _) = at_breakpoint();
        session.last_stop = Some(StopEvent::Signal { thread, signal: 11 });
        let step_request = if thread == T1 {
            ResumeRequest::step(T1).with_signal(T1, 11)
        } else {
            ResumeRequest::step(T1)
        };
        let final_request = if thread == T2 {
            ResumeRequest::continue_all().with_signal(T2, 11)
        } else {
            ResumeRequest::continue_all()
        };
        step(&mock, T1, BP + 4, step_request);
        mock.script(MockStop::new(StopEvent::Interrupted { thread: None }).expect_resume(final_request));
        session.resume_with_signal().unwrap();
        session.wait(None).unwrap();
    }
}

#[test]
fn memory_reads_overlay_partial_and_multiple_traps()
{
    let (mock, mut session, _) = at_breakpoint();
    session.add_breakpoint(software(BP + 4)).unwrap();
    for (address, len) in [(BP - 1, 2), (BP + 1, 2), (BP + 3, 3), (BP - 1, 10), (BP + 7, 2)] {
        assert_eq!(
            session.read_memory(Address::new(address), len).unwrap(),
            (address - CODE..address - CODE + len as u64)
                .map(|b| b as u8)
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn writes_preserve_armed_traps_and_update_disabled_originals_atomically()
{
    let (mock, mut session, a) = at_breakpoint();
    let b = session.add_breakpoint(software(BP + 4)).unwrap();
    session.disable_breakpoint(b).unwrap();
    let data = vec![77; 8];
    mock.fail_next(
        MockCallKind::WriteMemory,
        BackendError::Os {
            code: 5,
            message: "write failed".into(),
        },
    );
    assert!(session.write_memory(Address::new(BP + 1), &data).is_err());
    assert_eq!(session.read_memory(Address::new(BP), 4).unwrap(), original(BP));
    session.write_memory(Address::new(BP + 1), &data).unwrap();
    assert_eq!(peek(&mock, BP), TRAP);
    session.enable_breakpoint(b).unwrap();
    assert_eq!(peek(&mock, BP + 4), TRAP);
    session.disable_breakpoint(b).unwrap();
    assert_eq!(peek(&mock, BP + 4), vec![77; 4]);
    session.remove_breakpoint(a).unwrap();
    assert_eq!(peek(&mock, BP), vec![8, 77, 77, 77]);
}

#[test]
fn initial_step_start_failures_restore_trap_and_stay_stopped()
{
    for kind in [MockCallKind::WriteMemory, MockCallKind::Resume] {
        let (mock, mut session, _) = at_breakpoint();
        mock.fail_next(
            kind,
            BackendError::Os {
                code: 5,
                message: "start failed".into(),
            },
        );
        assert!(session.resume().is_err());
        assert_eq!(session.state(), SessionState::Stopped);
        assert!(session.step_plan.is_none());
        assert_eq!(peek(&mock, BP), TRAP);
    }
}

#[test]
fn final_continue_failure_is_stopped_without_last_stop()
{
    let (mock, mut session, _) = at_breakpoint();
    session.resume().unwrap();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    mock.fail_next(
        MockCallKind::Resume,
        BackendError::Os {
            code: 5,
            message: "continue failed".into(),
        },
    );
    assert!(session.wait(None).is_err());
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(session.last_stop().is_none());
    assert!(session.step_plan.is_none());
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn drop_attached_with_active_plan_restores_all_originals()
{
    let mock = process();
    let mut session = session(&mock);
    session.attach(ProcessId(42)).unwrap();
    session.add_breakpoint(software(BP)).unwrap();
    session.add_breakpoint(software(BP2)).unwrap();
    session.write_registers_for(T1, &regs_at(BP)).unwrap();
    session.resume().unwrap();
    drop(session);
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(peek(&mock, BP2), original(BP2));
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn timeout_between_serial_steps_preserves_the_next_step()
{
    let (mock, mut session, _) = at_breakpoint();
    session.write_registers_for(T2, &regs_at(BP)).unwrap();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    // T1's step completes inside `resume`, T2's does not arrive in time.
    session.resume().unwrap();
    assert_eq!(session.wait(Some(Duration::ZERO)).unwrap(), None);
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.step_plan.is_some());
    assert_eq!(peek(&mock, BP), original(BP));
    step(&mock, T2, BP + 4, ResumeRequest::step(T2));
    hit(&mock, BP2);
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn rearm_failure_disables_breakpoint_and_stops_session()
{
    let (mock, mut session, id) = at_breakpoint();
    session.resume().unwrap();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    mock.fail_next(
        MockCallKind::WriteMemory,
        BackendError::Os {
            code: 5,
            message: "rearm failed".into(),
        },
    );
    assert!(session.wait(None).is_err());
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(session.last_stop().is_none());
    assert!(!session.breakpoint_info(id).unwrap().enabled);
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn resume_finishes_the_step_over_and_continues_without_wait()
{
    let (mock, mut session, _) = at_breakpoint();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    session.resume().unwrap();
    // No `wait` yet: the program already runs past the breakpoint.
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.step_plan.is_none());
    assert_eq!(peek(&mock, BP), TRAP);
    assert_eq!(mock.state(), MockState::Running);
    assert_eq!(mock.calls().last(), Some(&MockCall::Resume(ResumeRequest::continue_all())));
}

#[test]
fn terminal_event_during_resume_is_reported_and_kill_still_works()
{
    for kill in [false, true] {
        let (mock, mut session, _) = at_breakpoint();
        mock.script(MockStop::new(StopEvent::Exited { code: 3 }).expect_resume(ResumeRequest::step(T1)));
        session.resume().unwrap();
        assert_eq!(session.state(), SessionState::Running);
        if kill {
            // The process already exited: kill reports that, not a SIGKILL.
            assert_eq!(session.kill().unwrap(), StopEvent::Exited { code: 3 });
            assert_eq!(session.last_stop(), Some(&StopEvent::Exited { code: 3 }));
        } else {
            assert_eq!(session.wait(None).unwrap(), Some(StopEvent::Exited { code: 3 }));
        }
        assert_eq!(session.state(), SessionState::Exited);
        assert_eq!(mock.state(), MockState::NoProcess);
    }
}

/// One stop reports the internal step and another thread's breakpoint: the breakpoint is
/// reported, nothing is resumed while it is pending.
#[test]
fn event_reported_with_the_internal_step_is_not_skipped()
{
    let (mock, mut session, _) = at_breakpoint();
    mock.script_simultaneous([
        MockStop::new(StopEvent::SingleStep { thread: T1 })
            .set_pc(T1, Address::new(BP + 4))
            .expect_resume(ResumeRequest::step(T1)),
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP2),
        }),
    ]);
    session.resume().unwrap();
    assert_eq!(peek(&mock, BP), TRAP);
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP2)
        })
    );
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(!mock.calls().contains(&MockCall::Resume(ResumeRequest::continue_all())));
    // Nothing else is pending and the next resume works.
    assert_eq!(session.wait(Some(Duration::ZERO)).unwrap(), None);
    hit(&mock, BP2);
    session.resume().unwrap();
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
}

/// Another event arrives before the internal step's completion in the same stop: the
/// late `SingleStep` is dropped and never reaches the caller.
#[test]
fn late_single_step_of_an_abandoned_step_is_dropped()
{
    let (mock, mut session, _) = at_breakpoint();
    let interrupted = StopEvent::Interrupted { thread: None };
    mock.script_simultaneous([
        MockStop::new(interrupted.clone()).expect_resume(ResumeRequest::step(T1)),
        MockStop::new(StopEvent::SingleStep { thread: T1 }).set_pc(T1, Address::new(BP + 4)),
    ]);
    session.resume().unwrap();
    assert_eq!(session.wait(None).unwrap(), Some(interrupted));
    assert_eq!(peek(&mock, BP), TRAP);
    assert_eq!(session.wait(Some(Duration::ZERO)).unwrap(), None);
    hit(&mock, BP2);
    session.resume().unwrap();
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
}

/// `resume_with_signal` passes a signal to T2 on the final continue; T1's step-over is cut
/// short by T1's own signal. T2's signal is still held, so the next `resume_with_signal`
/// passes it together with T1's.
#[test]
fn signal_for_another_thread_survives_an_abandoned_step_over()
{
    let (mock, mut session, _) = at_breakpoint();
    session.last_stop = Some(StopEvent::Signal { thread: T2, signal: 30 });
    let alarm = StopEvent::Signal { thread: T1, signal: 14 };
    mock.script(MockStop::new(alarm.clone()).expect_resume(ResumeRequest::step(T1)));
    session.resume_with_signal().unwrap();
    assert_eq!(session.wait(None).unwrap(), Some(alarm));

    // T1 is still at the breakpoint: it steps over with its own signal, and T2's signal
    // goes on the final continue.
    step(&mock, T1, BP + 4, ResumeRequest::step(T1).with_signal(T1, 14));
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP2),
        })
        .expect_resume(ResumeRequest::continue_all().with_signal(T2, 30)),
    );
    session.resume_with_signal().unwrap();
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
}

/// A plain `resume` discards a carried signal, as it discards every held signal.
#[test]
fn plain_resume_drops_a_carried_signal()
{
    let (mock, mut session, _) = at_breakpoint();
    session.last_stop = Some(StopEvent::Signal { thread: T2, signal: 30 });
    let alarm = StopEvent::Signal { thread: T1, signal: 14 };
    mock.script(MockStop::new(alarm.clone()).expect_resume(ResumeRequest::step(T1)));
    session.resume_with_signal().unwrap();
    assert_eq!(session.wait(None).unwrap(), Some(alarm));
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    hit(&mock, BP2);
    session.resume().unwrap();
    assert!(mock.calls().contains(&MockCall::Resume(ResumeRequest::continue_all())));
    session.wait(None).unwrap();
}

/// A step the caller asked for is reported as `SingleStep` even when an interrupt lost
/// the race, at a breakpoint as at a plain pc.
#[test]
fn requested_step_at_a_breakpoint_reports_single_step_despite_interrupt()
{
    let (mock, mut session, _) = at_breakpoint();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    session.step_instruction(T1).unwrap();
    session.interrupt_requested = true;
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: T1 }));
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn internal_wait_failure_is_deferred_until_wait()
{
    let (mock, mut session, _) = at_breakpoint();
    let error = BackendError::Os {
        code: 5,
        message: "internal wait failed".into(),
    };
    // The first wait checks for pending events before starting the step.
    mock.fail_on_nth(MockCallKind::Wait, 2, error);
    session.resume().unwrap();
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.step_plan.is_some());
    assert!(session.wait(None).unwrap_err().to_string().contains("internal wait failed"));
    assert_eq!(session.state(), SessionState::Running);
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    hit(&mock, BP2);
    assert!(matches!(session.wait(None).unwrap(), Some(StopEvent::Breakpoint { .. })));
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn final_continue_failure_inside_resume_leaves_session_stopped()
{
    let (mock, mut session, _) = at_breakpoint();
    step(&mock, T1, BP + 4, ResumeRequest::step(T1));
    mock.fail_on_nth(
        MockCallKind::Resume,
        2,
        BackendError::Os {
            code: 5,
            message: "final continue failed".into(),
        },
    );
    assert!(session.resume().unwrap_err().to_string().contains("final continue failed"));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.state(), MockState::Stopped);
    assert!(session.last_stop().is_none());
    assert!(session.step_plan.is_none());
    assert_eq!(peek(&mock, BP), TRAP);
}
