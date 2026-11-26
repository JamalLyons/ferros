//! Step-out: return-continue stops and temporary trap cleanup.

use ferros_core::backend::mock::{MockBackend, MockStop, arm64_registers};
use ferros_core::backend::{LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::types::ThreadId;
use ferros_core::{Session, arch};

use crate::helpers::*;

#[test]
fn resume_retries_failed_step_out_hook_cleanup()
{
    use ferros_core::backend::mock::MockCallKind;
    use ferros_core::backend::{BackendError, ImageRole, LoadedImage};
    use ferros_core::objfile::mock::MockObjectImage;
    use ferros_core::runtime::ExitSource;
    use ferros_core::runtime::mock::MockRuntime;
    use ferros_core::types::Address;
    use ferros_core::{BreakpointRequest, DebuggerError, SessionState};

    const BASE: u64 = 0x2_0000_0000;
    const USER: Address = Address::new(BASE + 0x400);
    const PRIVATE: Address = Address::new(BASE + 0x408);
    const SECOND_ENTRY: &str = "second_leaf";
    let symbols = MockObjectImage::new(USER.value()..USER.value() + 16, BASE + 0x1000, BASE + 0x1100)
        .with_symbol(USER.value(), EXIT_ENTRY)
        .with_symbol(PRIVATE.value(), SECOND_ENTRY);
    let mut image = symbols.header_bytes();
    image.resize(0x1000, 0);
    image.extend(symbols.table_bytes());
    image.resize(0x1100, 0);
    image.extend(symbols.name_bytes());
    let mock = returning_to_breakpoint()
        .with_region(Address::new(BASE), "r-x", image)
        .with_image(LoadedImage {
            path: format!("/usr/lib/{EXIT_IMAGE}").into(),
            load_address: Address::new(BASE),
            build_id: ferros_core::BuildIdentity::Unknown,
            role: ImageRole::Library,
        });
    let runtime = MockRuntime::new()
        .with_exit_source(ExitSource::named(EXIT_IMAGE, &[EXIT_ENTRY, SECOND_ENTRY]))
        .into_shared();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared()).with_runtime(runtime);
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let user = session
        .add_breakpoint(BreakpointRequest::Software { address: USER })
        .expect("add user breakpoint");
    session.disable_breakpoint(user).expect("disable user breakpoint");

    mock.script(MockStop::new(StopEvent::Interrupted { thread: Some(THREAD) }).expect_resume(step_out_continue()));
    session.step_out().expect("arm the return trap and both exit hooks");
    let trap = arch::Arm64::TRAP_INSTRUCTION.to_vec();
    assert_eq!(mock.peek_memory(USER, 4), Ok(trap.clone()));
    assert_eq!(mock.peek_memory(PRIVATE, 4), Ok(trap.clone()));

    // Restore the return trap first, then fail the write for the first exit hook.
    let error = BackendError::Os {
        code: 5,
        message: "injected hook restore failure".into(),
    };
    mock.fail_on_nth(MockCallKind::WriteMemory, 2, error.clone());
    assert!(matches!(session.wait(None), Err(DebuggerError::Backend(actual)) if actual == error));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.peek_memory(USER, 4), Ok(trap));
    assert_eq!(mock.peek_memory(PRIVATE, 4), Ok(vec![0; 4]));

    session.resume().expect("retry hook cleanup before resuming");
    assert_eq!(mock.peek_memory(USER, 4), Ok(vec![0; 4]), "the first hook was restored");
    assert_eq!(mock.peek_memory(PRIVATE, 4), Ok(vec![0; 4]));
    let breakpoints = session.breakpoints();
    assert_eq!(breakpoints.len(), 1, "only the user breakpoint remains");
    assert_eq!(breakpoints[0].id, user);
    assert!(!breakpoints[0].enabled, "the user breakpoint is disabled again");
}

fn step_out_with_failed_return_restore(mock: &MockBackend, queued: Option<MockStop>) -> Session
{
    use ferros_core::DebuggerError;
    use ferros_core::backend::BackendError;
    use ferros_core::backend::mock::MockCallKind;

    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("step-out-trap-adoption")).expect("launch");
    let interrupt = MockStop::new(StopEvent::Interrupted { thread: Some(THREAD) }).expect_resume(step_out_continue());
    mock.script_simultaneous(std::iter::once(interrupt).chain(queued));
    session.step_out().expect("arm the return trap");

    let error = BackendError::Os {
        code: 5,
        message: "injected return trap restore failure".into(),
    };
    mock.fail_next(MockCallKind::WriteMemory, error.clone());
    assert!(matches!(session.wait(None), Err(DebuggerError::Backend(actual)) if actual == error));
    session
}

#[test]
fn source_step_drops_queued_stop_at_line_return_trap_pending_cleanup()
{
    let peer = ThreadId(2);
    let mock = returning_to_breakpoint().with_thread(peer, arm64_registers());
    let queued = MockStop::new(StopEvent::Breakpoint {
        thread: peer,
        pc: BREAKPOINT,
    })
    .set_pc(peer, BREAKPOINT);
    let mut session = step_out_with_failed_return_restore(&mock, Some(queued));
    assert_eq!(mock.pending_len(), 1);
    session.select_thread(THREAD).expect("select the stepping thread");
    mock.script(MockStop::new(StopEvent::SingleStep { thread: THREAD }).expect_resume(ResumeRequest::step(THREAD)));

    session.step_into().expect("restore the return trap and step");
    assert_eq!(
        session.wait(None).expect("report the source step"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));
    assert_eq!(mock.peek_registers(peer).expect("peer registers").pc, BREAKPOINT);
    assert_eq!(mock.pending_len(), 0);
}

#[test]
fn step_out_cleanup_preserves_a_new_low_level_breakpoint()
{
    use ferros_core::BreakpointRequest;

    let mock = returning_to_breakpoint();
    let mut session = step_out_with_failed_return_restore(&mock, None);
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
        .expect("claim the existing return trap");
    session.resume().expect("retry cleanup before continuing");

    assert!(session.breakpoint_info(id).expect("claimed breakpoint survived").enabled);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

#[test]
fn step_out_cleanup_preserves_a_new_logical_breakpoint()
{
    use ferros_core::BreakpointLocation;

    let mock = returning_to_breakpoint();
    let mut session = step_out_with_failed_return_restore(&mock, None);
    let logical = session
        .set_breakpoint(BreakpointLocation::Address(BREAKPOINT))
        .expect("claim the existing return trap");
    session.resume().expect("retry cleanup before continuing");

    assert_eq!(session.logical_breakpoint_at(BREAKPOINT).map(|bp| bp.id), Some(logical));
    assert_eq!(
        mock.peek_memory(BREAKPOINT, 4),
        Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()),
        "the new logical breakpoint still traps"
    );
}

/// Script `step_out`'s return trap: `THREAD` reaches `BREAKPOINT` in its caller's frame.
fn script_return_trap(mock: &MockBackend, session: &mut Session)
{
    let caller = session.stack_trace(8).unwrap()[1].clone();
    let mut at_return = session.read_registers_for(THREAD).unwrap();
    at_return.pc = BREAKPOINT;
    at_return.sp = caller.sp;
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_registers(THREAD, at_return)
        .expect_resume(step_out_continue()),
    );
}

/// #148: two stale private-trap stops are queued behind an interrupt. `start_line_step`
/// drains the first and drops it; the second is still queued when `run_line_to` drains
/// before its continue, and it belongs to no request either: it is dropped and the step
/// runs to its return.
#[test]
fn step_out_drops_a_stale_trap_stop_queued_for_its_return_continue()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session.select_thread(THREAD).expect("select the stepping thread");
    step_out_interrupted_at_return_trap(&mock, &mut session, vec![stale_trap_stop(other, CALLEE + 16)]);
    assert_eq!(mock.pending_len(), 2);

    session
        .write_registers_for(THREAD, &in_callee(arm64_registers()))
        .expect("back in the callee");
    session.select_thread(THREAD).expect("select the stepping thread");
    script_return_trap(&mock, &mut session);
    session.step_out().expect("step out");
    assert_eq!(mock.pending_len(), 0, "both stale stops drained");
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: THREAD }));
    assert_eq!(session.read_registers_for(THREAD).unwrap().pc, BREAKPOINT);
    assert_eq!(mock.script_len(), 0);
    assert!(session.breakpoints().is_empty());
}

/// #148: the first stop `start_line_step` drains can be another thread's, from a private
/// trap that is gone; it belongs to no request either, so it is dropped and the step runs.
#[test]
fn step_out_drops_another_threads_stale_trap_stop_queued_before_it()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let interrupted = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.script_simultaneous(vec![
        MockStop::new(interrupted.clone()).expect_resume(ResumeRequest::continue_all()),
        stale_trap_stop(other, CALLEE + 16),
    ]);
    session.resume().expect("resume");
    assert_eq!(session.wait(None).unwrap(), Some(interrupted));
    assert_eq!(mock.pending_len(), 1);

    session.select_thread(THREAD).expect("select the stepping thread");
    script_return_trap(&mock, &mut session);
    session.step_out().expect("step out");
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: THREAD }));
    assert_eq!(session.read_registers_for(THREAD).unwrap().pc, BREAKPOINT);
    assert_eq!(mock.script_len(), 0);
    assert_eq!(mock.pending_len(), 0);
}

/// #148: a breakpoint stop at a real, armed trap queued at the same point is the caller's:
/// `run_line_to`'s drain still reports it, without running the step.
#[test]
fn step_out_reports_a_real_breakpoint_stop_queued_for_its_return_continue()
{
    use ferros_core::BreakpointRequest;

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

    session
        .write_registers_for(THREAD, &in_callee(arm64_registers()))
        .expect("back in the callee");
    session.select_thread(THREAD).expect("select the stepping thread");
    let _ = mock.take_calls();
    session.step_out().expect("step out");
    assert!(resumes(&mock).is_empty(), "the step did not run");
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]), "no private trap left");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint { thread: other, pc: user })
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    assert_eq!(mock.pending_len(), 0);
}

/// `step_out` from a user breakpoint at the stepping thread's pc, with another thread
/// `other`: the step first steps `THREAD` off that breakpoint (resume 1), then issues the
/// step's continue (resume 2) and runs to its return trap. Returns the breakpoint's id.
fn step_out_from_a_user_breakpoint(mock: &MockBackend, session: &mut Session) -> ferros_core::BreakpointId
{
    use ferros_core::BreakpointRequest;

    session.select_thread(THREAD).expect("select the stepping thread");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: CALLEE })
        .expect("breakpoint at the pc");
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, CALLEE + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    script_return_trap(mock, session);
    id
}

/// #148 review: a source step that starts at a user breakpoint steps off it, then issues
/// its continue. A stale private-trap stop of another thread that arrives just before
/// that continue makes the backend refuse it; it belongs to no request, so it is dropped
/// and the continue is issued again, instead of ending the step.
#[test]
fn step_out_from_a_breakpoint_drops_a_stale_trap_stop_that_refuses_its_continue()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let id = step_out_from_a_user_breakpoint(&mock, &mut session);
    mock.stop_before_nth_resume(2, std::iter::once(stale_trap_stop(other, CALLEE + 16)));
    let _ = mock.take_calls();
    session.step_out().expect("step out");
    assert_eq!(
        session.wait(None).expect("wait"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );
    assert_eq!(
        session.read_registers_for(THREAD).unwrap().pc,
        BREAKPOINT,
        "the step ran to its return"
    );
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::step(THREAD), step_out_continue(), step_out_continue()],
        "refused once, then run"
    );
    assert_eq!(
        mock.peek_memory(CALLEE, 4),
        Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()),
        "re-armed"
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
    assert_eq!(mock.script_len(), 0);
    assert_eq!(mock.pending_len(), 0);
}

/// `returning_to_breakpoint` with a second thread `other` and the program's own `brk #1`
/// at `CALLEE + 16`, where no breakpoint is set.
fn returning_to_breakpoint_with_program_trap(other: ThreadId) -> MockBackend
{
    let mut stack = vec![0u8; 32];
    stack[8..16].copy_from_slice(&BREAKPOINT.value().to_le_bytes());
    let mut code = vec![0x1F; 64];
    code[16..20].copy_from_slice(&BRK_1);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    MockBackend::new()
        .with_thread(THREAD, in_callee(arm64_registers()))
        .with_thread(other, registers)
        .with_region(CALLEE, "r-x", code)
        .with_region(STACK, "rw-", stack)
}

/// #148 review: another thread's breakpoint stop at the program's own `brk #1`, queued for
/// `start_line_step`, has no breakpoint-table entry but is a real stop: it is reported,
/// without running the step, not dropped as a stale private trap.
#[test]
fn step_out_reports_another_threads_program_trap_queued_before_it()
{
    let other = ThreadId(2);
    let program_trap = CALLEE + 16;
    let mock = returning_to_breakpoint_with_program_trap(other);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let interrupted = StopEvent::Interrupted { thread: Some(THREAD) };
    let trapped = StopEvent::Breakpoint {
        thread: other,
        pc: program_trap,
    };
    mock.script_simultaneous(vec![
        MockStop::new(interrupted.clone()).expect_resume(ResumeRequest::continue_all()),
        MockStop::new(trapped.clone()).set_pc(other, program_trap),
    ]);
    session.resume().expect("resume");
    assert_eq!(session.wait(None).unwrap(), Some(interrupted));
    assert_eq!(mock.pending_len(), 1);

    session.select_thread(THREAD).expect("select the stepping thread");
    let _ = mock.take_calls();
    session.step_out().expect("step out");
    assert!(resumes(&mock).is_empty(), "the step did not run");
    assert_eq!(session.wait(None).unwrap(), Some(trapped));
    assert_eq!(mock.peek_memory(program_trap, 4), Ok(BRK_1.to_vec()));
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]), "no private trap left");
    assert_eq!(mock.pending_len(), 0);
}

/// #148 review: the same program trap stop queued behind a stale private-trap stop is still
/// queued for `run_line_to`'s drain before the continue; it is reported there, without
/// running the step, while the stale stop before it is dropped.
#[test]
fn step_out_reports_a_program_trap_stop_queued_for_its_return_continue()
{
    let other = ThreadId(2);
    let program_trap = CALLEE + 16;
    let mock = returning_to_breakpoint_with_program_trap(other);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session.select_thread(THREAD).expect("select the stepping thread");
    let trapped = StopEvent::Breakpoint {
        thread: other,
        pc: program_trap,
    };
    step_out_interrupted_at_return_trap(
        &mock,
        &mut session,
        vec![MockStop::new(trapped.clone()).set_pc(other, program_trap)],
    );
    assert_eq!(mock.pending_len(), 2);

    session
        .write_registers_for(THREAD, &in_callee(arm64_registers()))
        .expect("back in the callee");
    session.select_thread(THREAD).expect("select the stepping thread");
    let _ = mock.take_calls();
    session.step_out().expect("step out");
    assert!(resumes(&mock).is_empty(), "the step did not run");
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]), "no private trap left");
    assert_eq!(session.wait(None).unwrap(), Some(trapped));
    assert_eq!(mock.pending_len(), 0);
}

/// #148 (from #150): an abandoned step-over's late `SingleStep` arrives only after
/// `step_out`'s drains, so the backend refuses the step's continue with `StopPending`. It
/// belongs to no request of the step, which ran nothing: it is dropped, the continue is
/// issued again and the step runs to its return.
#[test]
fn step_out_drops_an_abandoned_steps_completion_that_refuses_its_continue()
{
    use ferros_core::BreakpointRequest;

    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    // `resume` steps THREAD over the breakpoint at its pc; an interrupt is reported
    // first, so that step is abandoned and its completion is still to come.
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: CALLEE })
        .expect("breakpoint at the pc");
    let cut = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.script(MockStop::new(cut.clone()).expect_resume(ResumeRequest::step(THREAD)));
    session.resume().expect("step over the breakpoint");
    assert_eq!(session.wait(None).expect("wait"), Some(cut));
    session.remove_breakpoint(id).expect("remove the breakpoint");

    mock.stop_before_next_resume(std::iter::once(MockStop::new(StopEvent::SingleStep { thread: THREAD })));
    script_return_trap(&mock, &mut session);
    let _ = mock.take_calls();
    session.step_out().expect("step out");
    assert_eq!(
        session.wait(None).expect("wait"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );
    assert_eq!(
        session.read_registers_for(THREAD).unwrap().pc,
        BREAKPOINT,
        "the step ran to its return"
    );
    assert_eq!(
        resumes(&mock),
        vec![step_out_continue(), step_out_continue()],
        "refused once, then run"
    );
    assert_eq!(mock.script_len(), 0);
    assert_eq!(mock.pending_len(), 0);
}

/// #148: a source step that stops at its return trap in a deeper recursive frame continues
/// the thread again. A stale private-trap stop that arrives after the step's drains makes
/// the backend refuse that continue; it is dropped, as `run_line_to` drops one, and the
/// continue is issued again.
#[test]
fn step_out_drops_a_stale_trap_stop_that_refuses_its_recursive_continue()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session.select_thread(THREAD).expect("select the stepping thread");

    // The return trap is reached first in a deeper frame (another sp), then in the
    // guarded one. The continue from the deeper frame first steps off the trap; the
    // stale stop races that step (resume 2).
    let caller = session.stack_trace(8).unwrap()[1].clone();
    let mut deeper = session.read_registers_for(THREAD).unwrap();
    deeper.pc = BREAKPOINT;
    deeper.sp = caller.sp - 0x40;
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_registers(THREAD, deeper)
        .expect_resume(step_out_continue()),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    script_return_trap(&mock, &mut session);
    mock.stop_before_nth_resume(2, std::iter::once(stale_trap_stop(other, CALLEE + 16)));
    let _ = mock.take_calls();
    session.step_out().expect("step out");
    assert_eq!(
        session.wait(None).expect("wait"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );
    assert_eq!(session.read_registers_for(THREAD).unwrap().sp, caller.sp);
    let step = ResumeRequest::step(THREAD);
    assert_eq!(
        resumes(&mock),
        vec![step_out_continue(), step.clone(), step, step_out_continue()],
        "the refused step off the trap was issued again"
    );
    assert_eq!(mock.script_len(), 0);
    assert_eq!(mock.pending_len(), 0);
}
