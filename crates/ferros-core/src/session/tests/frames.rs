use super::*;

/// A thread whose frame pointer chain has one caller at `0x1010`.
fn two_frames() -> (MockBackend, Session)
{
    let mut registers = regs_at(CODE);
    registers.fp = Address::new(0x2000);
    registers.sp = Address::new(0x2000);
    let mut stack = vec![0u8; 16];
    stack[8..].copy_from_slice(&0x1010u64.to_le_bytes());
    let mock = MockBackend::new()
        .with_thread(T1, registers)
        .with_region(Address::new(CODE), "r-x", vec![0; 32])
        .with_region(Address::new(0x2000), "rw-", stack);
    let session = launched(&mock);
    (mock, session)
}

#[test]
fn select_frame_rejects_an_index_past_the_only_frame()
{
    let mock = process();
    let mut session = launched(&mock);
    assert_eq!(session.selected_frame(), 0);
    session.select_frame(0).expect("innermost");
    let error = session.select_frame(1).expect_err("one frame");
    assert!(error.to_string().contains("no frame 1"), "{error}");
    assert_eq!(session.selected_frame(), 0);
    assert_eq!(session.selected_stack_frame().expect("frame").pc, Address::new(CODE));
}

#[test]
fn select_frame_needs_a_stopped_process()
{
    let mock = process();
    {
        let mut idle = session(&mock);
        assert_eq!(idle.selected_frame(), 0);
        assert!(matches!(idle.select_frame(0), Err(DebuggerError::NotAttached)));
        assert!(matches!(idle.selected_stack_frame(), Err(DebuggerError::NotAttached)));
    }

    let mut session = launched(&mock);
    mock.script(MockStop::new(StopEvent::Interrupted { thread: None }));
    session.resume().expect("resume");
    assert_eq!(session.selected_frame(), 0);
    assert!(matches!(session.select_frame(0), Err(DebuggerError::NotStopped)));
    assert!(matches!(session.selected_stack_frame(), Err(DebuggerError::NotStopped)));
    session.wait(None).expect("wait");
    assert_eq!(session.selected_frame(), 0);
}

#[test]
fn outer_frame_resets_when_the_thread_is_selected_or_the_process_runs()
{
    let (mock, mut session) = two_frames();
    session.select_frame(1).expect("caller");
    assert_eq!(session.selected_frame(), 1);
    assert_eq!(session.selected_stack_frame().expect("caller").pc, Address::new(0x1010));
    let error = session.select_frame(2).expect_err("past the end");
    assert!(error.to_string().contains("no frame 2"), "{error}");
    assert_eq!(session.selected_frame(), 1);

    // Selecting the thread that is already current still returns to the innermost frame.
    session.select_thread(T1).expect("same thread");
    assert_eq!(session.selected_frame(), 0);
    assert_eq!(session.selected_stack_frame().expect("inner").pc, Address::new(CODE));

    session.select_frame(1).expect("caller again");
    mock.script(MockStop::new(StopEvent::Interrupted { thread: None }));
    session.resume().expect("resume");
    assert_eq!(session.selected_frame(), 0, "resume clears the selection before the stop");
    session.wait(None).expect("wait");
    assert_eq!(session.selected_frame(), 0);
    assert_eq!(session.selected_stack_frame().expect("inner").index, 0);
    assert_eq!(session.selected_stack_frame().expect("inner").pc, Address::new(CODE));
}

#[test]
fn step_instruction_resets_the_selected_frame()
{
    let (mock, mut session) = two_frames();
    session.select_frame(1).expect("caller");
    mock.script(MockStop::new(StopEvent::SingleStep { thread: T1 }).set_pc(T1, Address::new(CODE + 4)));
    session.step_instruction(T1).expect("step");
    assert_eq!(session.selected_frame(), 0);
    session.wait(None).expect("wait");
    assert_eq!(session.selected_frame(), 0);
}

fn framed_registers(pc: u64) -> Registers
{
    let mut registers = regs_at(pc);
    registers.fp = Address::new(0x2000);
    registers.sp = Address::new(0x2000);
    registers
}

/// Two threads, a breakpoint at `BP`, and one caller frame above each thread.
fn framed_process() -> MockBackend
{
    let mut stack = vec![0u8; 16];
    stack[8..].copy_from_slice(&0x1004u64.to_le_bytes());
    MockBackend::new()
        .with_pid(ProcessId(42))
        .with_thread(T1, framed_registers(BP))
        .with_thread(T2, framed_registers(CODE + 4))
        .with_region(Address::new(CODE), "r-x", (0u8..32).collect::<Vec<_>>())
        .with_region(Address::new(0x2000), "rw-", stack)
}

/// A peer stop is reported while a signal handler's return is still queued. Selecting a
/// caller and polling `wait` lets that return resume the process internally. The session
/// is running afterwards, so the selected frame is back at 0 and another selection is
/// refused.
#[test]
fn handler_return_resume_clears_a_frame_selected_after_the_peer_stop()
{
    let mock = framed_process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("breakpoint");
    session.add_breakpoint(software(BP2)).expect("peer breakpoint");
    session.write_registers_for(T1, &framed_registers(BP)).expect("registers");
    session.last_stop = Some(StopEvent::Signal { thread: T1, signal: 11 });

    let signal_resume = ResumeRequest::all(ThreadAction::Suspend)
        .with_thread(T1, ThreadAction::Continue)
        .with_signal(T1, 11);
    mock.script(
        MockStop::new(StopEvent::Signal { thread: T1, signal: 14 })
            .set_registers(T1, framed_registers(0x3000))
            .expect_resume(signal_resume),
    );
    session.resume_with_signal().expect("deliver the signal");
    let stopped = session.wait(None).expect("wait").expect("the handler stopped");
    assert!(matches!(stopped, StopEvent::Signal { signal: 14, .. }), "{stopped:?}");

    mock.script_simultaneous([
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP2),
        })
        .expect_resume(ResumeRequest::continue_all()),
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        })
        .set_registers(T1, framed_registers(BP))
        .expect_resume(ResumeRequest::continue_all()),
    ]);
    session.resume().expect("continue while the handler is stopped");
    let peer = session.wait(None).expect("wait").expect("the peer stop");
    assert!(matches!(peer, StopEvent::Breakpoint { thread: T2, .. }), "{peer:?}");
    assert_eq!(session.state(), SessionState::Stopped);

    session.select_frame(1).expect("outer frame");
    assert_eq!(session.selected_frame(), 1);
    assert_eq!(session.wait(Some(Duration::ZERO)).expect("poll"), None);
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.selected_frame(), 0);
    assert!(session.last_stop().is_none());
    assert!(matches!(session.select_frame(0), Err(DebuggerError::NotStopped)));
}
