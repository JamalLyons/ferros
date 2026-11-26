use super::*;

#[test]
fn wait_none_while_stopped_is_not_running_instead_of_blocking()
{
    let mock = process();
    let mut session = launched(&mock);
    assert!(matches!(session.wait(None), Err(DebuggerError::NotRunning)));
    // The backend was only polled.
    assert_eq!(mock.take_calls(), [MockCall::Wait(Some(Duration::ZERO))]);
    // A finite timeout is passed through.
    assert_eq!(session.wait(Some(Duration::from_millis(5))).expect("wait"), None);
    assert_eq!(mock.take_calls(), [MockCall::Wait(Some(Duration::from_millis(5)))]);
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn resume_to_breakpoint_stop_follows_thread_and_counts_hit()
{
    let mock = process();
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP),
        })
        .set_pc(T2, Address::new(BP))
        .expect_resume(ResumeRequest::continue_all()),
    );
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    assert_eq!(session.selected_thread(), Some(T1));

    session.resume().expect("resume");
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.last_stop().is_none());

    let event = session.wait(None).expect("wait").expect("event");
    let expected = StopEvent::Breakpoint {
        thread: T2,
        pc: Address::new(BP),
    };
    assert_eq!(event, expected);
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.last_stop(), Some(&expected));
    assert_eq!(session.selected_thread(), Some(T2));
    assert_eq!(session.read_registers().expect("registers").pc, Address::new(BP));
    assert_eq!(session.breakpoint_info(id).expect("info").hit_count, 1);
}

#[test]
fn disabled_breakpoint_hit_is_not_counted()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Breakpoint {
        thread: T1,
        pc: Address::new(BP),
    }));
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    session.disable_breakpoint(id).expect("disable");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.breakpoint_info(id).expect("info").hit_count, 0);
}

#[test]
fn selection_falls_back_to_first_thread_when_selected_thread_exits()
{
    let mock = process();
    mock.script(
        MockStop::new(StopEvent::Interrupted { thread: None })
            .remove_thread(T2)
            .add_thread(ThreadId(3), regs_at(CODE)),
    );
    let mut session = launched(&mock);
    session.select_thread(T2).expect("select");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.selected_thread(), Some(T1));
    assert_eq!(session.threads().expect("threads"), vec![T1, ThreadId(3)]);
}

#[test]
fn selection_is_kept_when_the_event_names_no_thread()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Interrupted { thread: None }));
    let mut session = launched(&mock);
    session.select_thread(T2).expect("select");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.selected_thread(), Some(T2));
}

#[test]
fn terminal_event_exits_clears_process_and_allows_relaunch()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Killed { signal: 9 }));
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Killed { signal: 9 }));

    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.pid(), None);
    assert_eq!(session.selected_thread(), None);
    assert_eq!(session.last_stop(), Some(&StopEvent::Killed { signal: 9 }));
    // The traps went away with the process: the table is cleared.
    assert!(session.breakpoints().is_empty());

    assert_eq!(session.launch(&spec()).expect("relaunch"), ProcessId(42));
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(session.last_stop().is_none());
    assert_eq!(session.selected_thread(), Some(T1));
    // The new breakpoint reads the bytes now in memory (the old trap, since the mock
    // keeps its memory across processes) and gets a fresh entry.
    let id = session.add_breakpoint(software(BP2)).expect("add after relaunch");
    assert_eq!(session.breakpoints().len(), 1);
    assert_eq!(session.breakpoints()[0].id, id);
}

#[test]
fn resume_drains_pending_events_first()
{
    let mock = process();
    mock.script_simultaneous([
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        }),
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP2),
        }),
    ]);
    let mut session = launched(&mock);
    let first = session.add_breakpoint(software(BP)).expect("add");
    let second = session.add_breakpoint(software(BP2)).expect("add");
    session.resume().expect("resume");
    let event = session.wait(None).expect("wait").expect("event");
    assert_eq!(event.thread(), Some(T1));
    assert_eq!(mock.pending_len(), 1);
    let _ = mock.take_calls();

    // The second event is still queued: resume drains it instead of resuming.
    session.resume().expect("resume with an event pending");
    assert_eq!(session.state(), SessionState::Running);
    assert!(!mock.calls().iter().any(|call| matches!(call, MockCall::Resume(_))));
    let event = session.wait(None).expect("wait").expect("event");
    assert_eq!(event.thread(), Some(T2));
    assert_eq!(session.selected_thread(), Some(T2));
    assert_eq!(session.breakpoint_info(first).expect("info").hit_count, 1);
    assert_eq!(session.breakpoint_info(second).expect("info").hit_count, 1);

    // Nothing pending now: resume reaches the backend.
    let _ = mock.take_calls();
    session.resume().expect("resume");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::ReadRegisters(T1),
            MockCall::ReadRegisters(T2),
            MockCall::Resume(ResumeRequest::continue_all())
        ]
    );
}

#[test]
fn wait_while_stopped_returns_pending_event()
{
    let mock = process();
    mock.script_simultaneous([
        MockStop::new(StopEvent::Interrupted { thread: Some(T1) }),
        MockStop::new(StopEvent::Signal { thread: T2, signal: 30 }),
    ]);
    let mut session = launched(&mock);
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(
        session.wait(None).expect("pending"),
        Some(StopEvent::Signal { thread: T2, signal: 30 })
    );
    assert_eq!(session.selected_thread(), Some(T2));
    assert!(matches!(session.wait(None), Err(DebuggerError::NotRunning)));
}

/// Continuing from a breakpoint stop must first step the stopped thread
/// over the trap with the original instruction restored.
#[test]
fn resume_from_breakpoint_steps_over_the_trap()
{
    let mock = process();
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        })
        .set_pc(T1, Address::new(BP))
        .expect_resume(ResumeRequest::continue_all()),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: T1 })
            .set_pc(T1, Address::new(BP + 4))
            .expect_resume(ResumeRequest::all(ThreadAction::Suspend).with_thread(T1, ThreadAction::Step)),
    );
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    session.wait(None).expect("breakpoint stop");
    session.resume().expect("resume from the breakpoint");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

#[test]
fn failed_thread_listing_after_a_stop_does_not_keep_a_dead_thread_selected()
{
    let mock = process();
    let mut session = launched(&mock);
    session.select_thread(T2).expect("select T2");
    mock.script(MockStop::new(StopEvent::Interrupted { thread: None }).remove_thread(T2));
    session.resume().expect("resume");
    mock.fail_next(
        MockCallKind::Threads,
        BackendError::Os {
            code: 5,
            message: "threads".into(),
        },
    );
    session.wait(Some(Duration::ZERO)).expect("wait").expect("an event");
    assert_eq!(session.selected_thread(), None, "T2 exited but is still selected");

    assert_eq!(session.threads().expect("threads"), vec![T1]);
    assert_eq!(session.selected_thread(), Some(T1));
}

#[test]
fn resume_with_signal_passes_a_held_signal_and_resume_discards_it()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Signal { thread: T2, signal: 11 }));
    mock.script(MockStop::new(StopEvent::Signal { thread: T2, signal: 11 }));
    mock.script(MockStop::new(StopEvent::Interrupted { thread: Some(T1) }));
    let mut session = launched(&mock);

    // First stop: pass the signal on.
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    let _ = mock.take_calls();
    session.resume_with_signal().expect("resume with signal");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::Resume(ResumeRequest::continue_all().with_signal(T2, 11))
        ]
    );

    // Second signal stop: plain resume discards it.
    session.wait(None).expect("wait");
    let _ = mock.take_calls();
    session.resume().expect("resume");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::Resume(ResumeRequest::continue_all())
        ]
    );

    // After a non-signal stop, resume_with_signal passes nothing.
    session.wait(None).expect("wait");
    let _ = mock.take_calls();
    session.resume_with_signal().expect("resume with signal");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::Resume(ResumeRequest::continue_all())
        ]
    );
}

#[test]
fn resume_with_signal_while_running_is_not_stopped()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    let _ = mock.take_calls();
    assert!(matches!(session.resume_with_signal(), Err(DebuggerError::NotStopped)));
    assert!(mock.calls().is_empty());
}

#[test]
fn wait_with_timeout_while_running_and_nothing_happens_stays_running()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    assert_eq!(session.wait(Some(Duration::from_millis(200))).expect("wait"), None);
    assert_eq!(session.state(), SessionState::Running);
}
