//! Lifecycle tests: `attached_drop`.

use std::time::Duration;

use ferros_core::arch::Arm64;
use ferros_core::backend::{Backend, BackendError, ResumeRequest};
use ferros_core::{BreakpointLocation, LaunchSpec, ProcessOwnership, SessionState, StopEvent};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness};

use super::{SIGUSR1, TIMEOUT, assert_reaped, process_state, signal};

#[test]
fn attached_session_drop_releases_ownership_and_restores_ports()
{
    harness::run(|t| {
        for running in [false, true] {
            let target = t.spawn(&fixture!("loop_counter"), &[]);
            let pid = target.pid();
            let before = diagnostics::exception_ports(pid).expect("ports before");
            let mut session = ferros_macos::new_session().expect("backend");
            session.attach(pid).expect("attach");
            assert_eq!(session.ownership(), Some(ProcessOwnership::Attached));
            if running {
                session.resume().expect("resume attached");
            }
            drop(session);
            assert!(!harness::process_is_gone(pid.0));
            assert_eq!(diagnostics::exception_ports(pid).expect("restored ports"), before);
            let mut again = ferros_macos::new_session().expect("backend");
            again.attach(pid).expect("attach again: previous session released tracing");
            again.detach().expect("detach again");
            assert_eq!(diagnostics::exception_ports(pid).expect("ports after second detach"), before);
        }
    });
}

#[test]
fn attached_session_drop_keeps_the_target_when_trap_restore_fails()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut target = t.spawn(&fixture, &[]);
        let pid = target.pid();
        let backend = MacOSBackend::new().expect("backend");
        let fail_write = backend.memory_write_fault_injector();
        let mut session = ferros_macos::session_with(backend);
        session.attach(pid).expect("attach");
        session
            .set_breakpoint(BreakpointLocation::Line {
                file: format!("{}.rs", fixture.name()),
                line: fixture.marker_line("tick_body"),
            })
            .expect("set breakpoint");
        session.resume().expect("resume");
        let Some(StopEvent::Breakpoint { pc, .. }) = session.wait(Some(TIMEOUT)).expect("wait for breakpoint") else {
            panic!("target did not stop at its breakpoint");
        };
        let ports = diagnostics::exception_ports(pid).expect("debugger exception ports");
        let before = diagnostics::process_suspend_counts(pid)
            .expect("suspend counts")
            .task
            .expect("task suspend count");
        assert!(before > 0, "breakpoint stop must suspend the task");

        fail_write();
        drop(session);

        assert_eq!(
            target.try_wait().expect("check target status"),
            None,
            "target exited after drop"
        );
        let after = diagnostics::process_suspend_counts(pid)
            .expect("target should still be alive")
            .task
            .expect("task suspend count after drop");
        assert_eq!(after, before, "session drop released the task suspension");
        let state = process_state(pid.0);
        assert!(state.contains('X'), "target is no longer traced: {state}");
        assert_eq!(diagnostics::exception_ports(pid).expect("ports after drop"), ports);
        assert_eq!(
            diagnostics::raw_memory(pid, pc, Arm64::TRAP_INSTRUCTION.len()).expect("read retained instruction"),
            Arm64::TRAP_INSTRUCTION
        );
        drop(target); // Harness kills and reaps the retained fixture.
        assert_reaped(pid.0);
    });
}

#[test]
fn external_kill_while_stopped_is_a_pending_terminal_event()
{
    harness::run(|t| {
        for interrupted in [false, true] {
            let mut d = t.launch(&fixture!("loop_counter"));
            let pid = d.pid().0;
            if interrupted {
                d.resume().expect("resume");
                d.interrupt().expect("interrupt");
                assert!(matches!(
                    d.wait(Some(TIMEOUT)).expect("interrupt stop"),
                    Some(StopEvent::Interrupted { .. })
                ));
            }
            signal(pid, "-9");
            // Give death notification time to become pending, without consuming it.
            assert_reaped(pid);
            assert!(matches!(
                d.backend_mut().resume(&ResumeRequest::continue_all()),
                Err(BackendError::InvalidState(_))
            ));
            assert!(matches!(d.backend_mut().detach(), Err(BackendError::NotFound(_))));
            assert_eq!(
                d.wait(Some(TIMEOUT)).expect("terminal event"),
                Some(StopEvent::Killed { signal: 9 })
            );
            assert_eq!(d.state(), SessionState::Exited);
        }
    });
}

#[test]
fn backend_drop_is_an_ownership_safety_net()
{
    harness::run(|t| {
        let mut backend = MacOSBackend::new().expect("backend");
        let launch_tag = t.tag();
        let pid = backend
            .launch(&LaunchSpec::new(fixture!("loop_counter").path()).arg(launch_tag.as_str()))
            .expect("launch");
        drop(backend);
        assert_reaped(pid.0);
        let target = t.spawn(&fixture!("loop_counter"), &[]);
        let mut attached = MacOSBackend::new().expect("backend");
        attached.attach(target.pid()).expect("attach");
        attached.resume(&ResumeRequest::continue_all()).expect("resume");
        drop(attached);
        assert!(!harness::process_is_gone(target.pid().0));
        let mut again = MacOSBackend::new().expect("backend");
        again.attach(target.pid()).expect("reattach after backend drop");
        again.detach().expect("detach");
    });
}

#[test]
fn kill_attached_process_reaps_and_discards_events()
{
    harness::run(|t| {
        for running in [false, true] {
            let target = t.spawn(&fixture!("loop_counter"), &[]);
            let mut backend = MacOSBackend::new().expect("backend");
            backend.attach(target.pid()).expect("attach");
            if running {
                backend.resume(&ResumeRequest::continue_all()).expect("resume");
            }
            backend.kill().expect("kill attached");
            assert_reaped(target.pid().0);
            assert!(matches!(
                backend.wait(Some(Duration::ZERO)),
                Err(BackendError::InvalidState(_))
            ));
        }
    });
}

/// A process killed from outside while the session holds its logical signal stop
/// still dies before the next wait and is reported as killed (#56).
#[test]
fn external_kill_while_a_signal_stop_is_held_is_reported()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let pid = d.pid().0;
        d.resume().expect("resume");
        signal(pid, "-USR1");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("signal stop"),
            Some(StopEvent::Signal { signal: SIGUSR1, .. })
        ));
        signal(pid, "-9");
        assert_reaped(pid);
        assert_eq!(
            d.wait(Some(TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
        assert_eq!(d.state(), SessionState::Exited);
    });
}

/// Early replies must also permit autonomous death for an attached tracee, and
/// the watcher must return it to its original parent before publishing death.
#[test]
fn external_kill_of_an_attached_signal_stop_is_reported()
{
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let mut session = ferros_macos::new_session().expect("backend");
        session.attach(pid).expect("attach");
        session.resume().expect("resume");
        signal(pid.0, "-USR1");
        assert!(matches!(
            session.wait(Some(TIMEOUT)).expect("signal stop"),
            Some(StopEvent::Signal { signal: SIGUSR1, .. })
        ));
        signal(pid.0, "-9");
        assert_eq!(
            session.wait(Some(TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
        assert_eq!(session.state(), SessionState::Exited);
        drop(target); // The original parent owns final reaping after the tracer wait.
        assert_reaped(pid.0);
    });
}

#[test]
fn external_kill_while_running_reports_only_the_terminal_event()
{
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &["100000", "--progress"]);
        let pid = d.pid().0;
        d.resume().expect("resume");
        assert!(d.wait_for_stdout("tick:", TIMEOUT), "fixture did not run");
        signal(pid, "-9");
        assert_reaped(pid);
        assert_eq!(
            d.wait(Some(TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
        assert_eq!(d.state(), SessionState::Exited);
    });
}
