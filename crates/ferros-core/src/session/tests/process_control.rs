use super::*;

#[test]
fn dropping_a_stopped_session_restores_every_trap()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let disabled = session.add_breakpoint(software(BP2)).expect("add second");
    session.disable_breakpoint(disabled).expect("disable");
    assert_eq!(mock.peek_memory(Address::new(BP), 4).expect("peek"), TRAP);

    drop(session);
    assert_eq!(mock.peek_memory(Address::new(BP), 4).expect("peek"), original(BP));
    assert_eq!(mock.peek_memory(Address::new(BP2), 4).expect("peek"), original(BP2));
}

// --- Lifecycle: interrupt, kill, signal pass-through and ownership on drop (T06, #32) ---

fn attached(mock: &MockBackend) -> Session
{
    let mut session = session(mock);
    session.attach(ProcessId(77)).expect("attach");
    let _ = mock.take_calls();
    session
}

fn kinds(calls: &[MockCall]) -> Vec<MockCallKind>
{
    calls.iter().map(MockCall::kind).collect()
}

#[test]
fn interrupt_while_running_reports_interrupted_and_stops()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    let _ = mock.take_calls();

    session.interrupt().expect("interrupt");
    assert_eq!(mock.take_calls(), [MockCall::Interrupt]);
    assert_eq!(session.state(), SessionState::Running, "the stop is reported by wait");
    assert_eq!(
        session.wait(Some(Duration::from_secs(1))).expect("wait"),
        Some(StopEvent::Interrupted { thread: None })
    );
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.last_stop(), Some(&StopEvent::Interrupted { thread: None }));
}

#[test]
fn interrupt_while_stopped_calls_no_backend()
{
    let mock = process();
    let mut session = launched(&mock);
    session.interrupt().expect("interrupt while stopped");
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn interrupt_failure_leaves_the_session_running()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    mock.fail_next(
        MockCallKind::Interrupt,
        BackendError::Os {
            code: 5,
            message: "no".into(),
        },
    );
    assert!(matches!(
        session.interrupt(),
        Err(DebuggerError::Backend(BackendError::Os { code: 5, .. }))
    ));
    assert_eq!(session.state(), SessionState::Running);
}

#[test]
fn interrupt_and_kill_without_a_process_are_not_attached()
{
    let mock = process();
    let mut session = session(&mock);
    assert!(matches!(session.interrupt(), Err(DebuggerError::NotAttached)));
    assert!(matches!(session.kill(), Err(DebuggerError::NotAttached)));
    assert!(mock.calls().is_empty());

    // After exit as well.
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.launch(&spec()).expect("launch");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.state(), SessionState::Exited);
    let _ = mock.take_calls();
    assert!(matches!(session.interrupt(), Err(DebuggerError::NotAttached)));
    assert!(matches!(session.kill(), Err(DebuggerError::NotAttached)));
    assert!(mock.calls().is_empty());
    // The exit event is kept as the last stop.
    assert_eq!(session.last_stop(), Some(&StopEvent::Exited { code: 0 }));
}

fn assert_killed(session: &Session)
{
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.last_stop(), Some(&StopEvent::Killed { signal: SIGKILL }));
    assert_eq!(session.pid(), None);
    assert_eq!(session.selected_thread(), None);
    assert!(session.breakpoints().is_empty());
}

#[test]
fn kill_while_stopped_exits_and_allows_relaunch()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();

    assert_eq!(session.kill().expect("kill"), StopEvent::Killed { signal: 9 });
    // No trap is restored: the memory that held it is gone.
    assert_eq!(mock.take_calls(), [MockCall::Kill]);
    assert_killed(&session);
    assert_eq!(mock.state(), MockState::NoProcess);

    assert_eq!(session.launch(&spec()).expect("relaunch"), ProcessId(42));
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(session.last_stop().is_none());
}

#[test]
fn kill_while_running_exits()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    let _ = mock.take_calls();

    session.kill().expect("kill while running");
    assert_eq!(mock.take_calls(), [MockCall::Kill]);
    assert_killed(&session);
}

#[test]
fn kill_failure_leaves_state_and_breakpoints_unchanged()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    mock.fail_next(MockCallKind::Kill, BackendError::PermissionDenied("no".into()));
    assert!(matches!(session.kill(), Err(DebuggerError::PermissionDenied(_))));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.pid(), Some(ProcessId(42)));
    assert!(session.breakpoint_info(id).expect("info").enabled);
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn dropping_a_launched_session_kills_the_process()
{
    // Stopped.
    let mock = process();
    let session = launched(&mock);
    drop(session);
    assert_eq!(kinds(&mock.take_calls()), [MockCallKind::Kill]);
    assert_eq!(mock.state(), MockState::NoProcess);

    // Running.
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    let _ = mock.take_calls();
    drop(session);
    assert_eq!(mock.take_calls(), [MockCall::Kill]);
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn dropping_an_attached_stopped_session_restores_traps_then_detaches()
{
    let mock = process();
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();

    drop(session);
    let calls = mock.take_calls();
    assert_eq!(kinds(&calls), [MockCallKind::WriteMemory, MockCallKind::Detach]);
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn dropping_an_attached_running_session_interrupts_restores_and_detaches()
{
    let mock = process();
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    let _ = mock.take_calls();

    drop(session);
    let calls = mock.take_calls();
    let kinds = kinds(&calls);
    assert_eq!(kinds.first(), Some(&MockCallKind::Interrupt), "{calls:?}");
    let write = kinds
        .iter()
        .position(|k| *k == MockCallKind::WriteMemory)
        .expect("trap restored");
    let wait = kinds.iter().position(|k| *k == MockCallKind::Wait).expect("stop awaited");
    assert!(wait < write, "{calls:?}");
    assert_eq!(kinds.last(), Some(&MockCallKind::Detach), "{calls:?}");
    assert!(!kinds.contains(&MockCallKind::Kill));
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn dropping_an_attached_running_session_without_a_stop_holds_the_target()
{
    let mock = process();
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    mock.fail_next(
        MockCallKind::Interrupt,
        BackendError::Os {
            code: 1,
            message: "interrupt failed".into(),
        },
    );
    let _ = mock.take_calls();

    drop(session);
    assert!(matches!(
        mock.take_calls().as_slice(),
        [MockCall::Interrupt, MockCall::Wait(Some(_)), MockCall::HoldAttachedOnDrop]
    ));
    assert_eq!(mock.state(), MockState::Stopped);
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn dropping_an_attached_running_session_drains_simultaneous_events_before_detaching()
{
    let mock = process();
    mock.script_simultaneous([
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        }),
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP),
        }),
    ]);
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    // The interrupt raced the scripted stops; either way every event is drained.
    drop(session);
    assert_eq!(mock.pending_len(), 0);
    assert_eq!(mock.state(), MockState::NoProcess);
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(mock.calls().last().map(MockCall::kind), Some(MockCallKind::Detach));
}

#[test]
fn dropping_an_attached_session_whose_process_exits_does_not_detach()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    let mut session = attached(&mock);
    session.resume().expect("resume");
    // The interrupt fails and the process exits instead: the drop sees it gone.
    mock.fail_next(
        MockCallKind::Interrupt,
        BackendError::Os {
            code: 1,
            message: "gone".into(),
        },
    );
    let _ = mock.take_calls();
    drop(session);
    let calls = mock.take_calls();
    assert!(!kinds(&calls).contains(&MockCallKind::Detach), "{calls:?}");
    assert_eq!(mock.state(), MockState::NoProcess);
}
