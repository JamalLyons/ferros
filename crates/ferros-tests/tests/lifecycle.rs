//! Process lifecycle through `ferros_core::Session` on the macOS backend.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferros_core::backend::{Backend, BackendError, ResumeRequest};
use ferros_core::{LaunchSpec, ProcessOwnership, Session, SessionState, StopEvent};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness};

const TIMEOUT: Duration = Duration::from_secs(5);
// Darwin signal numbers; the test harness deliberately uses no raw FFI.
const SIGUSR1: i32 = 30;

fn signal(pid: u32, name: &str)
{
    assert!(
        Command::new("/bin/kill")
            .args([name, &pid.to_string()])
            .status()
            .expect("kill command")
            .success()
    );
}

fn assert_reaped(pid: u32)
{
    assert!(
        harness::wait_until_gone(pid, TIMEOUT),
        "pid {pid} still exists (possibly a zombie)"
    );
}

#[test]
fn hello_exits_without_exposing_the_attach_sigstop()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let pid = d.pid().0;
        assert_eq!(d.ownership(), Some(ProcessOwnership::Launched));
        d.resume().expect("resume");
        // Also exercises the blocking wait: the harness bounds the whole test.
        assert_eq!(d.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
        assert_eq!(d.state(), SessionState::Exited);
        assert_reaped(pid);
    });
}

#[test]
fn wait_reports_exit_code()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("exit_code"));
        let pid = d.pid().0;
        d.resume().expect("resume");
        assert_eq!(d.wait(Some(TIMEOUT)).expect("wait"), Some(StopEvent::Exited { code: 7 }));
        assert_eq!(d.state(), SessionState::Exited);
        assert_reaped(pid);
    });
}

#[test]
fn kill_terminates_and_reaps_at_entry_and_while_running()
{
    harness::run(|t| {
        for running in [false, true] {
            let mut d = t.launch(&fixture!("loop_counter"));
            let pid = d.pid().0;
            if running {
                d.resume().expect("resume");
            }
            assert_eq!(d.kill().expect("kill"), StopEvent::Killed { signal: 9 });
            assert_eq!(d.last_stop(), Some(&StopEvent::Killed { signal: 9 }));
            assert_eq!(d.state(), SessionState::Exited);
            assert_reaped(pid);
        }
    });
}

#[test]
fn interrupt_then_continue_makes_progress()
{
    harness::run(|t| {
        // A short loop can finish only if both the launch and interrupt holds are released.
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &["50"]);
        d.resume().expect("resume");
        d.interrupt().expect("interrupt");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("wait"),
            Some(StopEvent::Interrupted { .. })
        ));
        d.resume().expect("continue");
        assert!(d.wait_for_stdout("final count: 50", TIMEOUT), "no progress after continue");
        assert_eq!(d.wait(Some(TIMEOUT)).expect("exit"), Some(StopEvent::Exited { code: 0 }));
    });
}

#[test]
fn running_wait_honours_its_timeout()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        d.resume().expect("resume");
        let start = Instant::now();
        assert_eq!(d.wait(Some(Duration::from_millis(200))).expect("wait"), None);
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(180), "wait returned early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "wait took {elapsed:?}");
        assert_eq!(d.state(), SessionState::Running);
        d.kill().expect("kill");
    });
}

/// Temporary non-executable fixture copy; removed even if an assertion panics.
struct NonExecutable(std::path::PathBuf);
impl Drop for NonExecutable
{
    fn drop(&mut self)
    {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn failed_launches_are_typed_and_leave_no_process()
{
    harness::run(|_| {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let tag = format!("ferros-fixture-nonexec-{}-{nonce}", std::process::id());
        let path = NonExecutable(std::env::temp_dir().join(&tag));
        let mut backend = MacOSBackend::new().expect("backend");
        assert!(matches!(
            backend.launch(&LaunchSpec::new(&path.0)),
            Err(BackendError::NotFound(_))
        ));
        fs::copy(fixture!("hello").path(), &path.0).expect("copy fixture");
        fs::set_permissions(&path.0, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(matches!(
            backend.launch(&LaunchSpec::new(&path.0)),
            Err(BackendError::PermissionDenied(_))
        ));
        assert_eq!(harness::tagged_pids(&tag), Vec::<u32>::new());
        // The same backend remains usable after both failures.
        let pid = backend
            .launch(&LaunchSpec::new(fixture!("hello").path()))
            .expect("valid launch");
        backend.kill().expect("kill");
        assert_reaped(pid.0);
    });
}

#[test]
fn session_drop_reaps_at_entry_and_while_running()
{
    harness::run(|t| {
        for running in [false, true] {
            let mut d = t.launch(&fixture!("loop_counter"));
            let pid = d.pid();
            let empty = Session::new(Box::new(MacOSBackend::new().expect("backend")));
            let mut session = std::mem::replace(d.session_mut(), empty);
            if running {
                session.resume().expect("resume");
            }
            drop(session);
            // Checked before dropping any harness guard: the session must do the reaping.
            assert_reaped(pid.0);
        }
    });
}

#[test]
fn unix_signal_can_be_suppressed_or_delivered()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let pid = d.pid().0;
        d.resume().expect("resume");
        for deliver in [false, true] {
            signal(pid, "-USR1");
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("signal stop"),
                Some(StopEvent::Signal { signal: SIGUSR1, .. })
            ));
            if deliver {
                d.resume_with_signal().expect("deliver signal");
                assert_eq!(
                    d.wait(Some(TIMEOUT)).expect("signal exit"),
                    Some(StopEvent::Killed { signal: SIGUSR1 })
                );
                assert_reaped(pid);
            } else {
                d.resume().expect("suppress signal");
                assert_eq!(d.wait(Some(Duration::from_millis(100))).expect("still running"), None);
                assert_eq!(d.state(), SessionState::Running);
            }
        }
    });
}

#[test]
fn attached_session_drop_releases_ownership_and_restores_ports()
{
    harness::run(|t| {
        for running in [false, true] {
            let target = t.spawn(&fixture!("loop_counter"), &[]);
            let pid = target.pid();
            let before = diagnostics::exception_ports(pid).expect("ports before");
            let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
            session.attach(pid).expect("attach");
            assert_eq!(session.ownership(), Some(ProcessOwnership::Attached));
            if running {
                session.resume().expect("resume attached");
            }
            drop(session);
            assert!(!harness::process_is_gone(pid.0));
            assert_eq!(diagnostics::exception_ports(pid).expect("restored ports"), before);
            let mut again = Session::new(Box::new(MacOSBackend::new().expect("backend")));
            again.attach(pid).expect("attach again: previous session released tracing");
            again.detach().expect("detach again");
            assert_eq!(diagnostics::exception_ports(pid).expect("ports after second detach"), before);
        }
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
        let pid = backend
            .launch(&LaunchSpec::new(fixture!("loop_counter").path()))
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
