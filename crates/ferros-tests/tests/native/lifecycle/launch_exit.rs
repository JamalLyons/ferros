//! Lifecycle tests: `launch_exit`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use ferros_core::backend::{Backend, BackendError};
use ferros_core::{LaunchSpec, ProcessOwnership, SessionState, StopEvent};
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness, scratch};

use super::{TIMEOUT, assert_reaped};

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
    harness::run(|t| {
        let path = NonExecutable(scratch::unique_path("ferros-fixture-nonexec", ""));
        let tag = path.0.file_name().expect("file name").to_string_lossy().into_owned();
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
        let launch_tag = t.tag();
        let pid = backend
            .launch(&LaunchSpec::new(fixture!("hello").path()).arg(launch_tag.as_str()))
            .expect("valid launch");
        backend.kill().expect("kill");
        assert_reaped(pid.0);
    });
}

/// A launch whose task acquisition fails after `posix_spawn` succeeded kills and reaps
/// the suspended child before returning the error: it leaves the process table, not
/// just the list of live tagged processes.
#[test]
fn launch_discards_a_child_whose_task_cannot_be_acquired()
{
    harness::run(|t| {
        let mut backend = MacOSBackend::new().expect("backend");
        let discarded = backend.launch_task_fault_injector();
        let tag = t.tag();
        assert!(matches!(
            backend.launch(&LaunchSpec::new(fixture!("loop_counter").path()).arg(tag.as_str())),
            Err(BackendError::PermissionDenied(_))
        ));
        let pid = discarded().expect("the fault fired after spawning a child");
        // Checked before dropping the tag: the backend must do the reaping.
        assert_reaped(pid.0);
        // The fault is one-shot and the backend remains usable.
        let launch_tag = t.tag();
        let pid = backend
            .launch(&LaunchSpec::new(fixture!("hello").path()).arg(launch_tag.as_str()))
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
            let empty = ferros_macos::new_session().expect("backend");
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
