//! Signals held while a deferred stop or a failed resume is in flight.

use std::time::Duration;

use ferros_core::backend::mock::{MockBackend, MockCallKind, MockState, MockStop, arm64_registers};
use ferros_core::backend::{BackendError, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::types::Address;
use ferros_core::{Session, SessionState};

use crate::helpers::*;

/// Leave the second signal stop deferred so the first signal must be carried.
fn defer_simultaneous_signal_stops() -> (MockBackend, Session, StopEvent)
{
    let mut registers = arm64_registers();
    registers.pc = BREAKPOINT;
    let mut peer_registers = registers.clone();
    peer_registers.pc = BREAKPOINT + 4;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_thread(SIGNAL_THREAD, peer_registers)
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let first = StopEvent::Signal {
        thread: THREAD,
        signal: 5,
    };
    let second = StopEvent::Signal {
        thread: SIGNAL_THREAD,
        signal: 6,
    };
    mock.script_simultaneous(vec![MockStop::new(first.clone()), MockStop::new(second.clone())]);
    session.resume().expect("run to both signal stops");
    assert_eq!(session.wait(None).expect("wait"), Some(first));
    let _ = mock.take_calls();
    session.resume_with_signal().expect("defer for the second signal");
    assert_eq!(resumes(&mock), Vec::<ResumeRequest>::new());
    (mock, session, second)
}

#[test]
fn pre_start_failure_still_delivers_both_held_signals()
{
    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    assert_eq!(session.wait(None).expect("wait"), Some(second));
    mock.fail_next(MockCallKind::Wait, BackendError::Unsupported("drain failed".into()));
    assert!(matches!(
        session.resume_with_signal(),
        Err(ferros_core::DebuggerError::Backend(BackendError::Unsupported(message))) if message == "drain failed"
    ));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(
        resumes(&mock),
        Vec::<ResumeRequest>::new(),
        "the failed drain attempted no resume"
    );

    let next = StopEvent::Interrupted { thread: None };
    mock.script(MockStop::new(next.clone()));
    session.resume_with_signal().expect("retry with both held signals");
    let both = ResumeRequest::continue_all()
        .with_signal(THREAD, 5)
        .with_signal(SIGNAL_THREAD, 6);
    assert_eq!(resumes(&mock), vec![both]);
    assert_eq!(session.wait(None).expect("wait"), Some(next));
    session.resume_with_signal().expect("continue after delivery");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
}

#[test]
fn invalid_second_resume_keeps_held_signals_during_a_deferred_stop()
{
    for invalid_resume in [Session::resume, Session::resume_with_signal] {
        let (mock, mut session, second) = defer_simultaneous_signal_stops();
        assert!(matches!(
            invalid_resume(&mut session),
            Err(ferros_core::DebuggerError::NotStopped)
        ));
        assert_eq!(session.wait(None).expect("wait"), Some(second));

        session.resume_with_signal().expect("deliver both held signals");
        let both = ResumeRequest::continue_all()
            .with_signal(THREAD, 5)
            .with_signal(SIGNAL_THREAD, 6);
        assert_eq!(resumes(&mock), vec![both]);
    }
}

#[test]
fn failed_plain_resume_drops_held_signals()
{
    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    assert_eq!(session.wait(None).expect("wait"), Some(second.clone()));
    mock.fail_next(MockCallKind::Resume, BackendError::Unsupported("resume failed".into()));
    assert!(session.resume().is_err());
    assert_eq!(session.last_stop(), Some(&second));
    assert_eq!(mock.state(), MockState::Stopped);
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);

    session.resume_with_signal().expect("continue after suppression");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
}

#[test]
fn source_step_register_failure_still_delivers_the_signal()
{
    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    assert_eq!(session.wait(None).expect("wait"), Some(second));
    session
        .select_thread(THREAD)
        .expect("select the thread with the carried signal");
    mock.fail_next(
        MockCallKind::ReadRegisters,
        BackendError::Unsupported("register read failed".into()),
    );
    assert!(session.step_over().is_err());
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(resumes(&mock), Vec::<ResumeRequest>::new());

    session.resume_with_signal().expect("deliver both held signals");
    let both = ResumeRequest::continue_all()
        .with_signal(THREAD, 5)
        .with_signal(SIGNAL_THREAD, 6);
    assert_eq!(resumes(&mock), vec![both]);
}

#[test]
fn failed_final_continue_retries_only_the_signal_for_the_suspended_thread()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    assert_eq!(session.wait(None).expect("wait"), Some(second));
    session
        .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint at the first thread's pc");
    session.resume_with_signal().expect("start the first thread's handler");
    let handler = ResumeRequest::all(ThreadAction::Suspend)
        .with_thread(THREAD, ThreadAction::Continue)
        .with_signal(THREAD, 5);
    assert_eq!(resumes(&mock), vec![handler]);

    mock.script(MockStop::new(StopEvent::Breakpoint {
        thread: THREAD,
        pc: BREAKPOINT,
    }));
    assert_eq!(session.wait(Some(Duration::ZERO)).expect("wait for the handler return"), None);
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(THREAD)]);
    mock.script(MockStop::new(StopEvent::SingleStep { thread: THREAD }).set_pc(THREAD, BREAKPOINT + 4));
    mock.fail_next(
        MockCallKind::Resume,
        BackendError::Unsupported("final continue failed".into()),
    );
    assert!(session.wait(None).is_err());
    assert_eq!(session.state(), SessionState::Stopped);
    let peer_only = ResumeRequest::continue_all().with_signal(SIGNAL_THREAD, 6);
    assert_eq!(resumes(&mock).as_slice(), std::slice::from_ref(&peer_only));

    session.resume_with_signal().expect("retry the final continue");
    assert_eq!(resumes(&mock), vec![peer_only]);
}

#[test]
fn deferred_plain_resume_suppresses_signals_when_the_handler_continue_runs()
{
    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-deferred-suppression"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);
    let signal = StopEvent::Signal {
        thread: THREAD,
        signal: 31,
    };
    mock.script_simultaneous(vec![MockStop::new(signal.clone()), handler_return(&registers)]);
    session.resume_with_signal().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    let _ = mock.take_calls();

    session.resume().expect("defer for the queued handler return");
    assert_eq!(resumes(&mock), Vec::<ResumeRequest>::new());
    let next = StopEvent::Interrupted { thread: None };
    mock.script(handler_step_off());
    mock.script(MockStop::new(next.clone()).expect_resume(ResumeRequest::continue_all()));
    assert_eq!(session.wait(None).expect("wait"), Some(next));
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::step(HANDLER), ResumeRequest::continue_all()]
    );

    session.resume_with_signal().expect("continue after suppression");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
}

/// Stop on the first signal, leaving the second queued in the backend.
fn queued_signal_stops() -> (MockBackend, Session, StopEvent)
{
    let mock = MockBackend::new()
        .with_thread(THREAD, arm64_registers())
        .with_thread(SIGNAL_THREAD, arm64_registers());
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let first = StopEvent::Signal {
        thread: THREAD,
        signal: 5,
    };
    let second = StopEvent::Signal {
        thread: SIGNAL_THREAD,
        signal: 6,
    };
    mock.script_simultaneous(vec![MockStop::new(first.clone()), MockStop::new(second.clone())]);
    session.resume().expect("run to both signal stops");
    assert_eq!(session.wait(None).expect("wait"), Some(first));
    let _ = mock.take_calls();
    (mock, session, second)
}

#[test]
fn deferred_plain_resume_drops_the_reported_signal()
{
    let (mock, mut session, second) = queued_signal_stops();
    session.resume().expect("defer for the second signal");
    assert!(resumes(&mock).is_empty(), "the deferred resume ran nothing");
    assert_eq!(session.wait(None).expect("wait"), Some(second));

    session.resume_with_signal().expect("deliver the second signal");
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::continue_all().with_signal(SIGNAL_THREAD, 6)]
    );
}

#[test]
fn plain_resume_drops_the_suspended_peers_signal_before_step_over()
{
    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    assert_eq!(session.wait(None).expect("wait"), Some(second));
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint at the first thread's pc");
    let interrupted = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.script(
        MockStop::new(interrupted.clone())
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    session.resume().expect("start the step-over");
    assert_eq!(session.wait(None).expect("wait"), Some(interrupted));
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(THREAD)]);

    session.resume_with_signal().expect("continue after suppression");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
}

/// A step deferred by a queued stop ran nothing, so the selected thread's signal is
/// still held, like a peer's (#150): a step drops it only once the step starts.
#[test]
fn deferred_steps_keep_the_selected_threads_signal()
{
    let steps: [fn(&mut Session) -> ferros_core::Result<()>; 4] = [
        |session| session.step_instruction(THREAD),
        Session::step_over,
        Session::step_into,
        Session::step_out,
    ];
    for step in steps {
        let (mock, mut session, second) = queued_signal_stops();
        step(&mut session).expect("defer the step for the second signal");
        assert!(resumes(&mock).is_empty(), "the deferred step ran nothing");
        assert_eq!(session.wait(None).expect("wait"), Some(second));

        session.resume_with_signal().expect("deliver both held signals");
        assert_eq!(
            resumes(&mock),
            vec![
                ResumeRequest::continue_all()
                    .with_signal(THREAD, 5)
                    .with_signal(SIGNAL_THREAD, 6)
            ]
        );
    }
}

#[test]
fn forwarding_discards_the_signal_of_a_departed_thread()
{
    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    assert_eq!(session.wait(None).expect("wait"), Some(second));
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint at the first thread's pc");
    let stop = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.script(
        MockStop::new(stop.clone())
            .set_pc(THREAD, BREAKPOINT + 4)
            .remove_thread(SIGNAL_THREAD),
    );
    session.resume_with_signal().expect("start the first thread's handler");
    assert_eq!(session.wait(None).expect("wait"), Some(stop));
    let _ = mock.take_calls();

    session.resume_with_signal().expect("continue the surviving thread");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
}

#[test]
fn thread_listing_failure_keeps_held_signals()
{
    let (mock, mut session, second) = defer_simultaneous_signal_stops();
    mock.fail_next(
        MockCallKind::Threads,
        BackendError::Unsupported("thread listing failed".into()),
    );
    assert_eq!(session.wait(None).expect("wait"), Some(second));

    session.resume_with_signal().expect("deliver both held signals");
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::continue_all()
                .with_signal(THREAD, 5)
                .with_signal(SIGNAL_THREAD, 6)
        ]
    );
}
