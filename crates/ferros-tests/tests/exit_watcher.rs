//! Exit-watcher errors must survive process death and must not prevent cleanup (#57).
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ferros_core::backend::{Backend, BackendError};
use ferros_core::{SessionState, StopEvent};
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness};

#[test]
fn an_external_reaper_error_is_reported_instead_of_disappearing_after_death()
{
    harness::run(|t| {
        let backend = MacOSBackend::new().expect("backend");
        backend.reply_fault_injector().reap_before_exit_watcher();
        let mut d = t.launch_with_backend(&fixture!("hello"), &[], backend);
        d.resume().expect("run to exit");
        let error = d
            .backend_mut()
            .wait(Some(Duration::from_secs(1)))
            .expect_err("watcher ECHILD must survive death");
        assert!(matches!(error, BackendError::Os { code: 10, .. }));
        let began = Instant::now();
        assert!(matches!(d.backend_mut().wait(None), Err(BackendError::Os { code: 10, .. })));
        assert!(began.elapsed() < Duration::from_secs(1));
        let _ = d.kill();
    });
}

#[test]
fn kill_releases_backend_and_session_after_a_watcher_reaping_error()
{
    harness::run(|t| {
        let backend = MacOSBackend::new().expect("backend");
        backend.reply_fault_injector().reap_before_exit_watcher();
        let mut d = t.launch_with_backend(&fixture!("loop_counter"), &[], backend);
        let pid = d.pid().0;
        assert!(matches!(
            d.kill(),
            Err(ferros_core::error::DebuggerError::Backend(BackendError::KillCleanup(_)))
        ));
        assert!(harness::wait_until_gone(pid, Duration::from_secs(2)));
        assert_eq!(d.state(), SessionState::Exited);
        assert_eq!(d.session().pid(), None);
        assert_eq!(d.ownership(), None);
        assert!(matches!(
            d.backend_mut().wait(Some(Duration::ZERO)),
            Err(BackendError::InvalidState(_))
        ));
        assert_ne!(d.last_stop(), Some(&StopEvent::Killed { signal: 9 }));
    });
}

#[test]
fn kill_cleanup_releases_thread_rights_and_allows_backend_reuse()
{
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &[]);
        let mut backend = MacOSBackend::new().expect("backend");
        backend.reply_fault_injector().reap_before_exit_watcher();
        backend.attach(target.pid()).expect("attach");
        assert_ne!(backend.thread_ports(), []);
        assert!(matches!(backend.kill(), Err(BackendError::KillCleanup(_))));
        assert!(backend.thread_ports().is_empty(), "thread rights retained after kill cleanup");
        assert!(matches!(
            backend.wait(Some(Duration::ZERO)),
            Err(BackendError::InvalidState(_))
        ));
        let next = t.spawn(&fixture!("loop_counter"), &[]);
        backend.attach(next.pid()).expect("reuse backend after cleanup error");
        backend.detach().expect("release replacement target");
    });
}
