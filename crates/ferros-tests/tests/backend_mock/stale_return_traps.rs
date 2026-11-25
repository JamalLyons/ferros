//! Stale return-trap stops and stops that arrive after a drain.

use ferros_core::backend::mock::{MockBackend, MockState, MockStop, arm64_registers};
use ferros_core::backend::{LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::types::{Address, ThreadId};
use ferros_core::{BreakpointLocation, Session, SessionState, arch};

use crate::helpers::*;

#[test]
fn enabling_a_breakpoint_at_a_line_step_site_keeps_it_enabled_and_armed()
{
    // Issue #133: a line step puts a temporary trap on a disabled site and disables the
    // site again when the step ends. The session is running for the whole step, so
    // enabling a logical breakpoint there is refused and the release cannot undo it.
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let id = session.set_breakpoint(BreakpointLocation::Address(BREAKPOINT)).unwrap();
    session.disable_logical_breakpoint(id).unwrap();
    step_out_to_breakpoint(&mock, &mut session);
    assert!(matches!(
        session.enable_logical_breakpoint(id),
        Err(ferros_core::DebuggerError::NotStopped)
    ));
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: THREAD }));
    assert!(!session.logical_breakpoints()[0].enabled);
    assert_eq!(session.logical_breakpoints()[0].hit_count, 0);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));

    // Enabled once the step has stopped, it is armed; the next line step through the
    // site reports the hit and leaves it enabled and armed.
    session.enable_logical_breakpoint(id).unwrap();
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    let back = in_callee(session.read_registers_for(THREAD).unwrap());
    session.write_registers_for(THREAD, &back).unwrap();
    step_out_to_breakpoint(&mock, &mut session);
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT
        })
    );
    assert!(session.logical_breakpoints()[0].enabled);
    assert_eq!(session.logical_breakpoints()[0].hit_count, 1);
    assert!(session.breakpoints()[0].enabled);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

/// Complement the real signal-handler step-resume test with the exact ordering: a user hit, a signal
/// interrupting the instruction step before it executes, then handler return to
/// that same pc. This ordering cannot be forced by sending a signal to a stopped
/// process: the kernel may deliver it after the instruction completes instead. The real
/// handler test is `tests/stepping.rs::signal_handler_return_does_not_count_the_step_resume_breakpoint`.
#[test]
fn signal_during_breakpoint_step_does_not_report_or_count_a_second_hit()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    let thread = ThreadId(1);
    let address = Address::new(0x1000);
    let next = address + 4;
    let mut registers = arm64_registers();
    registers.pc = address + 16;
    registers.sp = Address::new(0x8000);
    let mock = MockBackend::new()
        .with_thread(thread, registers)
        .with_region(address, "r-x", vec![0; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("scripted-signal-step")).expect("launch mock");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address })
        .expect("breakpoint");
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: address })
            .set_pc(thread, address)
            .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("run to user hit");
    assert!(matches!(
        session.wait(None).expect("wait").expect("stop"),
        StopEvent::Breakpoint { .. }
    ));
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    mock.script(MockStop::new(StopEvent::Signal { thread, signal: 30 }).expect_resume(ResumeRequest::step(thread)));
    session.resume().expect("signal interrupts original instruction");
    assert!(matches!(
        session.wait(None).expect("wait").expect("stop"),
        StopEvent::Signal { .. }
    ));
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: address })
            .set_pc(thread, address)
            .expect_resume(
                ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(thread, ThreadAction::Continue)
                    .with_signal(thread, 30),
            ),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread })
            .set_pc(thread, next)
            .expect_resume(ResumeRequest::step(thread)),
    );
    mock.script(MockStop::new(StopEvent::Interrupted { thread: Some(thread) }).expect_resume(ResumeRequest::continue_all()));
    session
        .resume_with_signal()
        .expect("handler returns, original instruction executes");
    assert_eq!(
        session.wait(None).expect("wait").expect("stop"),
        StopEvent::Interrupted { thread: Some(thread) }
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    assert_eq!(mock.peek_registers(thread).expect("registers").pc, next);
}

/// A signal passed at breakpoint A whose handler stops at breakpoint B leaves A's
/// step-resume rendezvous pending. Removing and re-adding A drops it, so the thread's
/// next arrival at A in the same frame is a real hit (issue #120 review).
#[test]
fn removing_the_breakpoint_drops_a_stopped_handlers_rendezvous()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    let thread = ThreadId(1);
    let original = Address::new(0x1000);
    let handler = Address::new(0x1020);
    let mut registers = arm64_registers();
    registers.pc = original;
    registers.sp = Address::new(0x8000);
    let mock = MockBackend::new()
        .with_thread(thread, registers.clone())
        .with_region(original, "r-x", vec![0; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-handler-stop"))
        .expect("launch mock");
    mock.script(MockStop::new(StopEvent::Signal { thread, signal: 30 }).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Signal { .. })));
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: original })
        .expect("breakpoint at the signal pc");
    let in_handler = session
        .add_breakpoint(BreakpointRequest::Software { address: handler })
        .expect("breakpoint in the handler");
    let mut handler_registers = arm64_registers();
    handler_registers.pc = handler;
    handler_registers.sp = Address::new(0x7000);
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: handler })
            .set_registers(thread, handler_registers)
            .expect_resume(
                ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(thread, ThreadAction::Continue)
                    .with_signal(thread, 30),
            ),
    );
    session.resume_with_signal().expect("pass the signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Breakpoint { pc, .. }) if pc == handler));

    session.remove_breakpoint(id).expect("remove the original breakpoint");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: original })
        .expect("re-add it");
    session.remove_breakpoint(in_handler).expect("remove the handler breakpoint");
    // The handler returns to the original pc, in the frame it interrupted.
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: original })
            .set_registers(thread, registers)
            .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("let the handler return");
    assert_eq!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { thread, pc: original })
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
}

#[test]
fn resume_drops_a_stale_return_trap_stop_and_continues()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    step_out_interrupted_at_return_trap(&mock, &mut session, Vec::new());

    // Resume drains the stale stop, then really continues the process.
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("resume");
    assert_eq!(mock.pending_len(), 0);
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(mock.script_len(), 0);
}

#[test]
fn step_instruction_drops_a_stale_return_trap_stop_and_steps()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    step_out_interrupted_at_return_trap(&mock, &mut session, Vec::new());

    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    session.step_instruction(THREAD).expect("step");
    assert_eq!(mock.pending_len(), 0);
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: THREAD }));
    assert_eq!(mock.script_len(), 0);
    assert_eq!(session.read_registers_for(THREAD).unwrap().pc, BREAKPOINT + 4);
}

#[test]
fn resume_still_reports_a_queued_stop_at_an_armed_breakpoint()
{
    use ferros_core::BreakpointRequest;

    // A second thread is stopped at a real breakpoint in the same group, queued behind
    // the stale return-trap stop. Only the stale one is dropped.
    let other = ThreadId(2);
    let user = CALLEE + 16;
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session.select_thread(THREAD).expect("select the stepping thread");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: user })
        .expect("user breakpoint");
    step_out_interrupted_at_return_trap(
        &mock,
        &mut session,
        vec![MockStop::new(StopEvent::Breakpoint { thread: other, pc: user }).set_pc(other, user)],
    );
    assert_eq!(session.breakpoints().len(), 1);

    session.resume().expect("resume");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint { thread: other, pc: user })
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    assert_eq!(mock.pending_len(), 0);
}

#[test]
fn resume_reports_a_drained_breakpoint_stop_at_an_armed_breakpoint()
{
    use ferros_core::BreakpointRequest;

    // Two threads stop at real breakpoints at once: `wait` reports the first, and the
    // second, drained by the next `resume`, still holds a trap, so it is reported too.
    let other = ThreadId(2);
    let first = CALLEE + 16;
    let second = CALLEE + 32;
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 40;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let a = session
        .add_breakpoint(BreakpointRequest::Software { address: first })
        .expect("first breakpoint");
    let b = session
        .add_breakpoint(BreakpointRequest::Software { address: second })
        .expect("second breakpoint");
    mock.script_simultaneous(vec![
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: first,
        })
        .set_pc(THREAD, first)
        .expect_resume(ResumeRequest::continue_all()),
        MockStop::new(StopEvent::Breakpoint {
            thread: other,
            pc: second,
        })
        .set_pc(other, second),
    ]);
    session.resume().expect("resume");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: THREAD,
            pc: first
        })
    );
    session.resume().expect("resume with a stop queued");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: other,
            pc: second
        })
    );
    assert_eq!(session.breakpoint_info(a).expect("first").hit_count, 1);
    assert_eq!(session.breakpoint_info(b).expect("second").hit_count, 1);
}

/// #150: a stop can arrive after `Session::resume` drained the backend's queue and before
/// the backend checks it, so the backend refuses the resume with `StopPending`. The
/// session defers that stop as it does a drained one: it becomes running, the next
/// `wait` reports the stop, and the step over the breakpoint at the pc is undone.
#[test]
fn resume_reports_a_stop_that_arrives_after_its_drain()
{
    let mock = scripted_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint");
    session.resume().expect("run to the breakpoint");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { .. })
    ));

    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    let _ = mock.take_calls();
    session.resume().expect("a stop raced the resume");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.state(), MockState::Stopped, "nothing ran");
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.wait(None).expect("wait"), Some(late.clone()));
    assert_eq!(session.last_stop(), Some(&late));
    assert_eq!(session.breakpoints()[0].hit_count, 1);

    // The process runs on from there: the breakpoint is stepped over and the exit reported.
    session.resume().expect("resume after the late stop");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150: the same race for `step_instruction`: the late stop is reported, not an error.
#[test]
fn step_instruction_reports_a_stop_that_arrives_after_its_drain()
{
    let mock = scripted_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.step_instruction(THREAD).expect("a stop raced the step");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    assert_eq!(mock.script_len(), 3, "no scripted stop was consumed");
}

/// #150: the same race for a source step that runs to a private return trap: the late
/// stop is reported and the private trap is gone with the step.
#[test]
fn step_out_reports_a_stop_that_arrives_after_its_drain()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.step_out().expect("a stop raced the step");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]), "private trap released");
    assert!(session.breakpoints().is_empty());
    assert_eq!(session.wait(None).expect("wait"), Some(late));
}

/// #150: the race can also hit the final continue of `resume`'s step over the breakpoint
/// at the pc: the late stop is reported from `wait`, the trap is armed again, and the
/// signal meant for the continue stays held for the next `resume_with_signal`.
#[test]
fn step_over_reports_a_stop_that_arrives_before_its_final_continue()
{
    let mock = scripted_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint");
    session.resume().expect("run to the breakpoint");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { .. })
    ));

    // Resume 1 is the step off the breakpoint (scripted SingleStep); resume 2 is the
    // final continue, which the late stop races.
    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(late.clone())));
    session.resume().expect("a stop raced the final continue");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.state(), MockState::Stopped, "the final continue ran nothing");
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    assert_eq!(session.breakpoints()[0].hit_count, 1);
    assert_eq!(mock.script_len(), 1, "only the exit is left");
}
