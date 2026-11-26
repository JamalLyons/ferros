use super::*;

#[test]
fn session_is_send()
{
    fn assert_send<T: Send>() {}
    assert_send::<Session>();
}

#[test]
fn launch_selects_first_thread_and_has_no_last_stop()
{
    let mock = process();
    let mut session = session(&mock);
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.pid(), None);
    assert_eq!(session.architecture(), Architecture::Arm64);

    assert_eq!(session.launch(&spec()).expect("launch"), ProcessId(42));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.pid(), Some(ProcessId(42)));
    assert_eq!(session.selected_thread(), Some(T1));
    assert!(session.last_stop().is_none());
    assert_eq!(session.threads().expect("threads"), vec![T1, T2]);

    session.select_thread(T2).expect("select");
    assert_eq!(session.read_registers().expect("registers").pc, Address::new(CODE + 4));
    assert!(matches!(
        session.select_thread(ThreadId(9)),
        Err(DebuggerError::ThreadNotFound(9))
    ));
    assert_eq!(session.selected_thread(), Some(T2));
}

#[test]
fn attach_records_pid_and_stops()
{
    let mock = process();
    let mut session = session(&mock);
    session.attach(ProcessId(77)).expect("attach");
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.pid(), Some(ProcessId(77)));
    assert_eq!(session.selected_thread(), Some(T1));
    assert!(session.last_stop().is_none());
    assert_eq!(mock.calls()[0], MockCall::Attach(ProcessId(77)));
}

#[test]
fn detach_restores_every_trap()
{
    let mock = process();
    let mut session = launched(&mock);
    let first = session.add_breakpoint(software(BP)).expect("add");
    session.add_breakpoint(software(BP2)).expect("add");
    let disabled = session.add_breakpoint(software(CODE)).expect("add");
    session.disable_breakpoint(disabled).expect("disable");
    assert_eq!(peek(&mock, BP2), TRAP.to_vec());
    assert_eq!(session.breakpoints().len(), 3);
    assert_eq!(session.breakpoints()[0].id, first);

    session.detach().expect("detach");
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(peek(&mock, BP2), original(BP2));
    assert_eq!(peek(&mock, CODE), original(CODE));
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.pid(), None);
    assert_eq!(session.selected_thread(), None);
    assert!(session.breakpoints().is_empty());
    assert_eq!(mock.state(), MockState::NoProcess);
    assert_eq!(mock.calls().last(), Some(&MockCall::Detach));

    // A new process can be debugged afterwards.
    session.launch(&spec()).expect("relaunch");
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn detach_keeps_the_process_when_a_restore_fails()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    mock.fail_next(
        MockCallKind::WriteMemory,
        BackendError::Os {
            code: 1,
            message: "write".into(),
        },
    );
    assert!(matches!(
        session.detach(),
        Err(DebuggerError::Backend(BackendError::Os { code: 1, .. }))
    ));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.state(), MockState::Stopped);
    assert!(!mock.calls().contains(&MockCall::Detach));
    assert!(session.breakpoint_info(id).expect("still there").enabled);
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
}

/// Every call that needs a process, with the error it must return.
fn assert_all_refused(session: &mut Session, expect: fn(&DebuggerError) -> bool, what: &str)
{
    let a = Address::new(CODE);
    let checks: Vec<(&str, Option<DebuggerError>)> = vec![
        ("detach", session.detach().err()),
        ("resume", session.resume().err()),
        ("step_instruction", session.step_instruction(T1).err()),
        ("threads", session.threads().err()),
        ("select_thread", session.select_thread(T1).err()),
        ("read_registers", session.read_registers().err()),
        ("read_registers_for", session.read_registers_for(T1).err()),
        ("write_registers", session.write_registers(&arm64_registers()).err()),
        (
            "write_registers_for",
            session.write_registers_for(T1, &arm64_registers()).err(),
        ),
        ("read_memory", session.read_memory(a, 4).err()),
        ("write_memory", session.write_memory(a, &[0]).err()),
        ("memory_regions", session.memory_regions().err()),
        ("add_breakpoint", session.add_breakpoint(software(BP)).err()),
        (
            "add_breakpoint(hardware)",
            session.add_breakpoint(BreakpointRequest::Hardware { address: a }).err(),
        ),
        (
            "remove_breakpoint",
            session.remove_breakpoint(BreakpointId::from_raw(1)).err(),
        ),
        (
            "enable_breakpoint",
            session.enable_breakpoint(BreakpointId::from_raw(1)).err(),
        ),
        (
            "disable_breakpoint",
            session.disable_breakpoint(BreakpointId::from_raw(1)).err(),
        ),
        ("stack_trace", session.stack_trace(8).err()),
        ("symbolicate", session.symbolicate(a).err()),
        ("images", session.images().err()),
    ];
    for (call, error) in checks {
        let error = error.unwrap_or_else(|| panic!("{call} succeeded {what}"));
        assert!(expect(&error), "{call} {what}: {error:?}");
    }
}

#[test]
fn calls_before_launch_are_not_attached_and_reach_no_backend()
{
    let mock = process();
    let mut session = session(&mock);
    assert_all_refused(&mut session, |e| matches!(e, DebuggerError::NotAttached), "before launch");
    assert!(matches!(session.wait(Some(Duration::ZERO)), Err(DebuggerError::NotAttached)));
    assert!(matches!(session.wait(None), Err(DebuggerError::NotAttached)));
    assert!(matches!(
        session.breakpoint_info(BreakpointId::from_raw(1)),
        Err(DebuggerError::NotAttached)
    ));
    assert!(session.breakpoints().is_empty());
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::NotStarted);
}

#[test]
fn calls_while_running_are_not_stopped_and_reach_no_backend()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    assert_eq!(session.state(), SessionState::Running);
    let _ = mock.take_calls();

    assert_all_refused(&mut session, |e| matches!(e, DebuggerError::NotStopped), "while running");
    assert!(matches!(session.launch(&spec()), Err(DebuggerError::AlreadyAttached)));
    assert!(matches!(session.attach(ProcessId(5)), Err(DebuggerError::AlreadyAttached)));
    // Reading the table is fine while running.
    assert!(session.breakpoint_info(id).expect("info").enabled);
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::Running);

    // Waiting is legal; the empty script reports nothing.
    assert_eq!(session.wait(Some(Duration::ZERO)).expect("wait"), None);
    assert_eq!(session.state(), SessionState::Running);
}

#[test]
fn launch_and_attach_while_stopped_are_already_attached()
{
    let mock = process();
    let mut session = launched(&mock);
    assert!(matches!(session.launch(&spec()), Err(DebuggerError::AlreadyAttached)));
    assert!(matches!(session.attach(ProcessId(5)), Err(DebuggerError::AlreadyAttached)));
    assert!(mock.calls().is_empty());
}

#[test]
fn calls_after_exit_are_not_attached_and_reach_no_backend()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    let mut session = launched(&mock);
    session.resume().expect("resume");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    let _ = mock.take_calls();

    assert_all_refused(&mut session, |e| matches!(e, DebuggerError::NotAttached), "after exit");
    assert!(matches!(session.wait(None), Err(DebuggerError::NotAttached)));
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::Exited);
}

#[test]
fn permission_denied_from_launch_surfaces_as_permission_denied()
{
    let mock = process();
    mock.fail_next(MockCallKind::Launch, BackendError::PermissionDenied("task_for_pid".into()));
    let mut session = session(&mock);
    let Err(DebuggerError::PermissionDenied(message)) = session.launch(&spec()) else {
        panic!("expected PermissionDenied")
    };
    assert_eq!(message, "task_for_pid");
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.pid(), None);
    session.launch(&spec()).expect("second launch");
}

#[test]
fn backend_error_conversion()
{
    assert!(matches!(
        DebuggerError::from(BackendError::PermissionDenied("x".into())),
        DebuggerError::PermissionDenied(m) if m == "x"
    ));
    let error = DebuggerError::from(BackendError::NotFound("thread 3".into()));
    assert_eq!(error.to_string(), "not found: thread 3");
    assert!(matches!(error, DebuggerError::Backend(BackendError::NotFound(_))));
}

#[test]
fn ownership_follows_launch_attach_detach_and_kill()
{
    let mock = process();
    let mut session = session(&mock);
    assert_eq!(session.ownership(), None);
    session.launch(&spec()).expect("launch");
    assert_eq!(session.ownership(), Some(ProcessOwnership::Launched));
    session.kill().expect("kill");
    assert_eq!(session.ownership(), None);
    session.attach(ProcessId(77)).expect("attach");
    assert_eq!(session.ownership(), Some(ProcessOwnership::Attached));
    session.detach().expect("detach");
    assert_eq!(session.ownership(), None);

    // A terminal event ends ownership too, for a launched and an attached process.
    for attach in [false, true] {
        mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
        if attach {
            session.attach(ProcessId(77)).expect("attach");
        } else {
            session.launch(&spec()).expect("launch");
        }
        session.resume().expect("resume");
        session.wait(None).expect("wait");
        assert_eq!(session.state(), SessionState::Exited);
        assert_eq!(session.ownership(), None, "attach = {attach}");
    }
}
