//! Lifecycle: launch, detach, kill and failure injection on the mock backend.

use ferros_core::arch;
use ferros_core::backend::mock::{MockBackend, MockCall, MockCallKind, MockState};
use ferros_core::backend::{Backend, BackendError, BackendResult, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;

use crate::helpers::*;

/// Continue past one breakpoint the way an engine would, through any `Backend`.
fn continue_past_breakpoint(backend: &mut dyn Backend) -> BackendResult<Vec<StopEvent>>
{
    let arch = arch::for_architecture(backend.architecture())?;
    let trap = arch.trap_instruction();
    let mut events = Vec::new();

    backend.launch(&LaunchSpec::new("/bin/prog"))?;
    let original = backend.read_memory(BREAKPOINT, trap.len())?;
    backend.write_memory(BREAKPOINT, trap)?;
    backend.resume(&ResumeRequest::continue_all())?;
    events.extend(backend.wait(None)?);
    let registers = backend.read_registers(THREAD)?;
    assert_eq!(arch.pc(&registers), BREAKPOINT);
    backend.write_memory(BREAKPOINT, &original)?;
    backend.resume(&ResumeRequest::step(THREAD))?;
    events.extend(backend.wait(None)?);
    backend.write_memory(BREAKPOINT, trap)?;
    backend.resume(&ResumeRequest::continue_all())?;
    events.extend(backend.wait(None)?);
    Ok(events)
}

#[test]
fn boxed_mock_runs_on_another_thread()
{
    let mock = scripted_process();
    let mut backend: Box<dyn Backend> = Box::new(mock.clone());
    let events = std::thread::spawn(move || continue_past_breakpoint(backend.as_mut()))
        .join()
        .expect("engine thread panicked")
        .expect("backend call failed");

    assert_eq!(
        events,
        vec![
            StopEvent::Breakpoint {
                thread: THREAD,
                pc: BREAKPOINT
            },
            StopEvent::SingleStep { thread: THREAD },
            StopEvent::Exited { code: 0 },
        ]
    );
    assert_eq!(mock.state(), MockState::NoProcess);
    assert_eq!(mock.script_len(), 0);
}

#[test]
fn breakpoint_step_exit_records_exact_calls()
{
    let mut mock = scripted_process();
    continue_past_breakpoint(&mut mock).expect("backend call failed");

    let trap = arch::Arm64::TRAP_INSTRUCTION.to_vec();
    assert_eq!(
        mock.take_calls(),
        vec![
            MockCall::Launch(LaunchSpec::new("/bin/prog")),
            MockCall::ReadMemory {
                address: BREAKPOINT,
                len: 4
            },
            MockCall::WriteMemory {
                address: BREAKPOINT,
                data: trap.clone()
            },
            MockCall::Resume(ResumeRequest::continue_all()),
            MockCall::Wait(None),
            MockCall::ReadRegisters(THREAD),
            MockCall::WriteMemory {
                address: BREAKPOINT,
                data: vec![0x1F; 4]
            },
            MockCall::Resume(ResumeRequest::step(THREAD)),
            MockCall::Wait(None),
            MockCall::WriteMemory {
                address: BREAKPOINT,
                data: trap.clone()
            },
            MockCall::Resume(ResumeRequest::continue_all()),
            MockCall::Wait(None),
        ]
    );
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(trap));
}

#[test]
fn detach_cleanup_error_clears_session_but_ordinary_error_keeps_it_stopped()
{
    use ferros_core::backend::BackendError;
    use ferros_core::backend::mock::MockCallKind;
    use ferros_core::{BreakpointRequest, Session, SessionState};

    for teardown in [false, true] {
        let mock = scripted_process();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        session
            .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
            .expect("breakpoint");
        let error = if teardown {
            BackendError::DetachCleanup("restore failed".into())
        } else {
            BackendError::Os {
                code: 1,
                message: "detach refused".into(),
            }
        };
        mock.fail_next(MockCallKind::Detach, error.clone());
        let returned = session.detach().expect_err("injected detach error");
        assert!(matches!(returned, ferros_core::error::DebuggerError::Backend(ref actual) if *actual == error));
        if teardown {
            assert_eq!(session.state(), SessionState::NotStarted);
            assert_eq!(mock.state(), MockState::NoProcess);
            assert_eq!(session.pid(), None);
            assert_eq!(session.ownership(), None);
            assert_eq!(session.selected_thread(), None);
            assert!(session.breakpoints().is_empty());
            session.launch(&LaunchSpec::new("/bin/prog")).expect("reuse session");
        } else {
            assert_eq!(session.state(), SessionState::Stopped);
            assert_eq!(mock.state(), MockState::Stopped);
            assert!(session.breakpoints().iter().all(|bp| !bp.enabled));
            session.detach().expect("retry detach");
        }
    }
}

#[test]
fn kill_cleanup_error_clears_session_but_ordinary_error_preserves_it()
{
    use ferros_core::backend::BackendError;
    use ferros_core::backend::mock::MockCallKind;
    use ferros_core::{BreakpointRequest, Session, SessionState};

    for teardown in [false, true] {
        let mock = scripted_process();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        session
            .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
            .expect("breakpoint");
        let error = if teardown {
            BackendError::KillCleanup("reaping failed".into())
        } else {
            BackendError::PermissionDenied("kill refused".into())
        };
        mock.fail_next(MockCallKind::Kill, error);
        session.kill().expect_err("injected kill failure");
        if teardown {
            assert_eq!(session.state(), SessionState::Exited);
            assert_eq!(mock.state(), MockState::NoProcess);
            assert_eq!(session.pid(), None);
            assert_eq!(session.ownership(), None);
            assert_eq!(session.selected_thread(), None);
            assert_eq!(session.last_stop(), None);
            assert!(session.breakpoints().is_empty());
            session.launch(&LaunchSpec::new("/bin/prog")).expect("reuse session");
        } else {
            assert_eq!(session.state(), SessionState::Stopped);
            assert_eq!(mock.state(), MockState::Stopped);
            assert!(session.breakpoints().iter().all(|bp| bp.enabled));
            session.kill().expect("retry kill");
        }
    }
}

#[test]
fn public_nth_failure_is_one_shot_and_counts_matching_calls()
{
    let mut mock = scripted_process();
    mock.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let error = BackendError::PermissionDenied("injected".into());
    mock.fail_on_nth(MockCallKind::ReadMemory, 2, error.clone());
    assert!(mock.read_memory(BREAKPOINT, 4).is_ok());
    assert!(mock.threads().is_ok());
    assert_eq!(mock.read_memory(BREAKPOINT, 4), Err(error));
    assert!(mock.read_memory(BREAKPOINT, 4).is_ok());
}

#[test]
fn session_carries_process_group_to_backend()
{
    use ferros_core::Session;
    use ferros_core::backend::ProcessGroup;

    for group in [None, Some(ProcessGroup::New)] {
        let mock = MockBackend::new();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        let mut spec = LaunchSpec::new("/bin/prog");
        assert_eq!(spec.process_group, None, "default must inherit");
        if let Some(group) = group {
            spec = spec.process_group(group);
        }
        session.launch(&spec).expect("launch");
        assert_eq!(spec.process_group, group);
        assert_eq!(mock.take_calls().first(), Some(&MockCall::Launch(spec)));
    }
}
