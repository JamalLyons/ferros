//! Signals carried across refused resumes, steps and stopped handlers.

use ferros_core::backend::mock::{MockBackend, MockCallKind, MockStop, arm64_registers};
use ferros_core::backend::{BackendError, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::types::{Address, ThreadId};
use ferros_core::{Session, SessionState, arch};

use crate::helpers::*;

/// #150 review: a `resume_with_signal` refused for a stop that arrived after its drain
/// sent nothing, so the signal it meant to pass is still held: the next
/// `resume_with_signal` passes it, together with the late stop's.
#[test]
fn signal_of_a_refused_resume_is_passed_by_the_next_resume_with_signal()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = Address::new(0x1_0000_0020);
    let mock = MockBackend::new()
        .with_thread(THREAD, registers.clone())
        .with_thread(other, registers)
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let first = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.script(MockStop::new(first.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(first));

    let late = StopEvent::Signal {
        thread: other,
        signal: 31,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.resume_with_signal().expect("a stop raced the resume");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    let _ = mock.take_calls();
    let both = ResumeRequest::continue_all().with_signal(THREAD, 30).with_signal(other, 31);
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("pass both signals");
    assert_eq!(resumes(&mock), vec![both]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// A fault holds no signal: a forwarding continue after a fault on one thread passes
/// only the signal still held for its peer, and a fault the thread raises again (the
/// instruction is retried) is reported again without any signal (#219).
#[test]
fn forwarding_continue_after_a_fault_passes_only_the_peers_signal()
{
    use ferros_core::FaultKind;

    let mock = MockBackend::new()
        .with_thread(THREAD, arm64_registers())
        .with_thread(SIGNAL_THREAD, arm64_registers())
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let held = StopEvent::Signal {
        thread: SIGNAL_THREAD,
        signal: 30,
    };
    let fault = StopEvent::Fault {
        thread: THREAD,
        kind: FaultKind::MemoryAccess,
        detail: "scripted".into(),
    };
    assert_eq!(fault.deliverable_signal(), None);
    mock.script(MockStop::new(held.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(held));
    // The peer's signal is carried: the fault races the forwarding resume.
    mock.stop_before_next_resume(std::iter::once(MockStop::new(fault.clone())));
    session.resume_with_signal().expect("a fault raced the resume");
    assert_eq!(session.wait(None).expect("wait"), Some(fault.clone()));
    let _ = mock.take_calls();

    mock.script(MockStop::new(fault.clone()));
    session.resume_with_signal().expect("continue after the fault");
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::continue_all().with_signal(SIGNAL_THREAD, 30)]
    );
    assert_eq!(session.wait(None).expect("wait"), Some(fault));

    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("continue after the repeated fault");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
}

/// #150 review: a source step's final continue (which passes no signal) refused for a
/// late stop keeps a signal already carried for a suspended thread, from an earlier
/// step-over that another stop cut short; the next `resume_with_signal` passes it.
#[test]
fn refused_step_out_keeps_a_signal_carried_for_another_thread()
{
    use ferros_core::BreakpointRequest;

    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let signal = StopEvent::Signal {
        thread: other,
        signal: 30,
    };
    mock.script(MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));

    // The step over the breakpoint at THREAD's pc is cut short, so the signal meant for
    // the final continue is carried.
    session
        .add_breakpoint(BreakpointRequest::Software { address: CALLEE })
        .expect("breakpoint at the pc");
    let cut = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.script(MockStop::new(cut.clone()).expect_resume(ResumeRequest::step(THREAD)));
    session.resume_with_signal().expect("step over the breakpoint");
    assert_eq!(session.wait(None).expect("wait"), Some(cut));

    // step_out steps THREAD over its breakpoint (resume 1); the late stop races its final
    // continue (resume 2), which suspends the other thread and passes no signal.
    session.select_thread(THREAD).expect("select");
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, CALLEE + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    let late = StopEvent::Interrupted { thread: Some(other) };
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(late.clone())));
    session.step_out().expect("step out");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    let _ = mock.take_calls();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("pass the carried signal");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all().with_signal(other, 30)]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150 review: a step over a breakpoint whose start is refused for a late stop ran
/// nothing, so a stopped handler's rendezvous at that breakpoint is kept: its later return
/// there is not reported or counted as a second hit.
#[test]
fn refused_step_over_keeps_a_stopped_handlers_rendezvous()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    // `handler` is the thread whose signal handler stops; `stepper` later sits at the
    // same breakpoint and is stepped over it.
    let stepper = ThreadId(1);
    let handler = ThreadId(2);
    let original = Address::new(0x1000);
    let in_handler = Address::new(0x1020);
    let mut registers = arm64_registers();
    registers.pc = original;
    registers.sp = Address::new(0x8000);
    let mut elsewhere = registers.clone();
    elsewhere.pc = original + 16;
    let mock = MockBackend::new()
        .with_thread(stepper, elsewhere)
        .with_thread(handler, registers.clone())
        .with_region(original, "r-x", vec![0; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("scripted-refused-step")).expect("launch");
    mock.script(
        MockStop::new(StopEvent::Signal {
            thread: handler,
            signal: 30,
        })
        .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("run to the signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Signal { .. })));
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: original })
        .expect("breakpoint at the signal pc");
    let handler_bp = session
        .add_breakpoint(BreakpointRequest::Software { address: in_handler })
        .expect("breakpoint in the handler");
    let mut handler_registers = registers.clone();
    handler_registers.pc = in_handler;
    handler_registers.sp = Address::new(0x7000);
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: handler,
            pc: in_handler,
        })
        .set_registers(handler, handler_registers)
        .expect_resume(
            ResumeRequest::all(ThreadAction::Suspend)
                .with_thread(handler, ThreadAction::Continue)
                .with_signal(handler, 30),
        ),
    );
    session.resume_with_signal().expect("pass the signal");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { pc, .. }) if pc == in_handler
    ));
    session.remove_breakpoint(handler_bp).expect("remove the handler breakpoint");

    // The other thread arrives at the original breakpoint; the step over it is refused.
    let mut at_original = session.read_registers_for(stepper).expect("registers");
    at_original.pc = original;
    session.write_registers_for(stepper, &at_original).expect("move");
    let late = StopEvent::Interrupted { thread: Some(stepper) };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.resume().expect("a stop raced the step over");
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    assert_eq!(mock.peek_memory(original, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));

    // The user moves the other thread off the breakpoint (a step over it that runs would
    // remove the trap, which gives the rendezvous up). The handler then returns to the
    // original breakpoint in the frame it interrupted: the rendezvous, so its instruction
    // is executed and the continue goes on to the interrupt.
    let mut moved = session.read_registers_for(stepper).expect("registers");
    moved.pc = original + 16;
    session.write_registers_for(stepper, &moved).expect("move off");
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: handler,
            pc: original,
        })
        .set_registers(handler, registers)
        .expect_resume(ResumeRequest::continue_all()),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: handler })
            .set_pc(handler, original + 4)
            .expect_resume(ResumeRequest::step(handler)),
    );
    let end = StopEvent::Interrupted { thread: Some(handler) };
    mock.script(MockStop::new(end.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(end));
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
    assert_eq!(mock.script_len(), 0);
}

/// #150 review: an instruction step that fails to start ran nothing, so the stepping
/// thread's carried signal is still held, as is a peer's, and the next
/// `resume_with_signal` passes both.
#[test]
fn failed_step_instruction_keeps_the_threads_carried_signal()
{
    let mock = MockBackend::new()
        .with_thread(THREAD, arm64_registers())
        .with_thread(SIGNAL_THREAD, arm64_registers())
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let signal = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.script(MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    // The signal is carried: a late stop refuses the resume that would pass it.
    let late = StopEvent::Signal {
        thread: SIGNAL_THREAD,
        signal: 6,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.resume_with_signal().expect("a stop raced the resume");
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    let _ = mock.take_calls();

    mock.fail_next(
        MockCallKind::Resume,
        BackendError::Os {
            code: 5,
            message: "step failed".into(),
        },
    );
    assert!(matches!(
        session.step_instruction(THREAD),
        Err(ferros_core::DebuggerError::Backend(BackendError::Os { code: 5, .. }))
    ));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(THREAD)]);

    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("pass both carried signals");
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::continue_all()
                .with_signal(THREAD, 30)
                .with_signal(SIGNAL_THREAD, 6)
        ]
    );
}

/// #150 re-review: a step over a breakpoint at another address than a stopped handler's
/// rendezvous, refused for a late stop or failing to start, leaves that rendezvous alone:
/// the handler's return to its own breakpoint is still not reported or counted.
#[test]
fn unstarted_step_over_elsewhere_keeps_a_stopped_handlers_rendezvous()
{
    use ferros_core::BreakpointRequest;

    for refused in [true, false] {
        let mock = handler_stopped_process();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        session.launch(&LaunchSpec::new("scripted-step-elsewhere")).expect("launch");
        let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

        // The other thread arrives at a different breakpoint; the step over it runs
        // nothing.
        let elsewhere = INTERRUPTED + 8;
        session
            .add_breakpoint(BreakpointRequest::Software { address: elsewhere })
            .expect("breakpoint elsewhere");
        let mut at_elsewhere = session.read_registers_for(THREAD).expect("registers");
        at_elsewhere.pc = elsewhere;
        session.write_registers_for(THREAD, &at_elsewhere).expect("move");
        if refused {
            let late = StopEvent::Interrupted { thread: Some(THREAD) };
            mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
            session.resume().expect("a stop raced the step over");
            assert_eq!(session.wait(None).expect("wait"), Some(late));
        } else {
            mock.fail_next(
                MockCallKind::Resume,
                BackendError::Os {
                    code: 5,
                    message: "step failed".into(),
                },
            );
            assert!(session.resume().is_err());
            assert_eq!(session.state(), SessionState::Stopped);
        }
        assert_eq!(mock.peek_memory(elsewhere, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));

        // The other thread is moved off; the handler returns to its breakpoint in the frame
        // it interrupted: the rendezvous, so the continue goes on to the interrupt.
        let mut moved = session.read_registers_for(THREAD).expect("registers");
        moved.pc = INTERRUPTED + 16;
        session.write_registers_for(THREAD, &moved).expect("move off");
        mock.script(
            MockStop::new(StopEvent::Breakpoint {
                thread: HANDLER,
                pc: INTERRUPTED,
            })
            .set_registers(HANDLER, registers)
            .expect_resume(ResumeRequest::continue_all()),
        );
        mock.script(
            MockStop::new(StopEvent::SingleStep { thread: HANDLER })
                .set_pc(HANDLER, INTERRUPTED + 4)
                .expect_resume(ResumeRequest::step(HANDLER)),
        );
        let end = StopEvent::Interrupted { thread: Some(HANDLER) };
        mock.script(MockStop::new(end.clone()).expect_resume(ResumeRequest::continue_all()));
        session.resume().expect("let the handler return");
        assert_eq!(session.wait(None).expect("wait"), Some(end), "refused: {refused}");
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
        assert_eq!(mock.script_len(), 0);
    }
}

/// #150 re-review: another thread's signal stop is reported before the stopped handler's
/// return to its breakpoint, which stays queued. `resume_with_signal` for that signal is
/// deferred for the queued return, so the signal stays carried; the session then consumes
/// the return as the rendezvous and continues every thread in place of that resume. The
/// continue passes the carried signal (its thread runs, so it would lose it otherwise),
/// and it is not carried any more for a later `resume_with_signal`.
#[test]
fn handler_return_continue_passes_a_signal_carried_from_a_deferred_resume()
{
    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("scripted-deferred-signal")).expect("launch");
    let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let signal = StopEvent::Signal {
        thread: THREAD,
        signal: 31,
    };
    mock.script_simultaneous(vec![
        MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()),
        MockStop::new(StopEvent::Breakpoint {
            thread: HANDLER,
            pc: INTERRUPTED,
        })
        .set_registers(HANDLER, registers),
    ]);
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    assert_eq!(mock.pending_len(), 1, "the handler's return is queued");

    let _ = mock.take_calls();
    session.resume_with_signal().expect("deferred for the queued return");
    assert_eq!(resumes(&mock), Vec::<ResumeRequest>::new(), "nothing was resumed");
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: HANDLER })
            .set_pc(HANDLER, INTERRUPTED + 4)
            .expect_resume(ResumeRequest::step(HANDLER)),
    );
    let end = StopEvent::Interrupted { thread: Some(HANDLER) };
    let with_signal = ResumeRequest::continue_all().with_signal(THREAD, 31);
    mock.script(MockStop::new(end.clone()).expect_resume(with_signal.clone()));
    assert_eq!(session.wait(None).expect("wait"), Some(end));
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(HANDLER), with_signal]);
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);

    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("nothing is carried any more");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150 re-review: a step-over's final continue that passes another thread's signal,
/// refused for a late stop, sent nothing: the next `resume_with_signal` passes that signal.
#[test]
fn refused_step_over_final_continue_keeps_another_threads_signal()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = BREAKPOINT;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers.clone())
        .with_thread(other, registers)
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let mut elsewhere = session.read_registers_for(other).expect("registers");
    elsewhere.pc = BREAKPOINT + 16;
    session.write_registers_for(other, &elsewhere).expect("move");
    let signal = StopEvent::Signal {
        thread: other,
        signal: 31,
    };
    mock.script(MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint at THREAD's pc");

    // Resume 1 steps THREAD over the breakpoint; the late stop races resume 2, the final
    // continue that passes the other thread's signal.
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    let late = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(late.clone())));
    session.resume_with_signal().expect("a stop raced the final continue");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    let _ = mock.take_calls();
    let pass = ResumeRequest::continue_all().with_signal(other, 31);
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(pass.clone()));
    session.resume_with_signal().expect("pass the held signal");
    assert_eq!(resumes(&mock), vec![pass]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}
