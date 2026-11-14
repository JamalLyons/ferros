//! Private ptrace handshake races (#57).
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferros_core::backend::Backend;
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness};

struct Observer(PathBuf);
impl Observer
{
    fn new() -> Self
    {
        static NEXT_OBSERVER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferros-signals-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT_OBSERVER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("observer directory");
        Self(path)
    }
    fn wait_released_after_drop(&self, pid: ferros_core::ProcessId)
    {
        let progress_before = std::fs::read_to_string(self.0.join("progress"));
        let began = Instant::now();
        // Both handlers must run after release. A missing acknowledgement alone
        // cannot distinguish signal loss from a task left suspended (#76).
        while !self.0.join("handled").exists() || !self.0.join("continued").exists() {
            if began.elapsed() >= Duration::from_secs(2) {
                let state = std::process::Command::new("/bin/ps")
                    .args(["-M", "-o", "pid,ppid,stat,time,command"])
                    .arg("-p")
                    .arg(pid.0.to_string())
                    .output()
                    .expect("target state");
                eprintln!("release acknowledgement deadline elapsed: {:?}", began.elapsed());
                eprintln!(
                    "handled={}, continued={}",
                    self.0.join("handled").exists(),
                    self.0.join("continued").exists()
                );
                eprintln!("progress before wait: {progress_before:?}");
                eprintln!("progress after wait: {:?}", std::fs::read_to_string(self.0.join("progress")));
                eprintln!("target: {}", String::from_utf8_lossy(&state.stdout));
                panic!("missing release acknowledgement");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn wait(&self, file: &str)
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.0.join(file).exists() {
            assert!(Instant::now() < deadline, "missing {file} acknowledgement");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for Observer
{
    fn drop(&mut self)
    {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn attach_preserves_a_signal_racing_its_private_stop_without_queuing_an_event()
{
    harness::run(|t| {
        for separate in [true, false] {
            let observer = Observer::new();
            let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
            observer.wait("ready");
            let mut backend = MacOSBackend::new().expect("backend");
            backend.reply_fault_injector().race_next_private_stop(separate);
            let began = Instant::now();
            backend.attach(target.pid()).expect("attach despite racing signal");
            assert!(began.elapsed() < Duration::from_secs(2));
            assert_eq!(backend.wait(Some(Duration::ZERO)).expect("no attach event"), None);
            backend
                .resume(&ferros_core::backend::ResumeRequest::continue_all())
                .expect("resume");
            observer.wait("handled");
            backend.kill().expect("kill");
        }
    });
}

#[test]
fn detach_preserves_a_new_signal_racing_its_private_stop()
{
    harness::run(|t| {
        for separate in [true, false] {
            let observer = Observer::new();
            let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
            observer.wait("ready");
            let mut backend = MacOSBackend::new().expect("backend");
            backend.attach(target.pid()).expect("attach");
            backend.reply_fault_injector().race_next_private_stop(separate);
            let began = Instant::now();
            backend.detach().expect("detach despite racing signal");
            assert!(began.elapsed() < Duration::from_secs(2));
            observer.wait("handled");
        }
    });
}

#[test]
fn detach_releases_the_target_even_if_a_racing_signal_cannot_be_replayed()
{
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let mut backend = MacOSBackend::new().expect("backend");
        backend.attach(target.pid()).expect("attach");
        let probe = backend.reply_fault_injector();
        probe.race_next_private_stop(true);
        probe.fail_next_handshake_signal_requeue();
        assert!(matches!(
            backend.detach(),
            Err(ferros_core::backend::BackendError::DetachCleanup(_))
        ));
        observer.wait("continued");
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &target.pid().0.to_string()])
                .status()
                .expect("fresh signal")
                .success()
        );
        observer.wait("handled");
    });
}

/// A real signal stop has reached the loop, but wait has not delivered it: the
/// backend still considers itself Running. This is the G6 drop ordering (#57).
#[test]
fn dropping_a_running_backend_with_a_pending_stop_releases_the_target_promptly()
{
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let mut backend = MacOSBackend::new().expect("backend");
        let probe = backend.reply_fault_injector();
        backend.attach(target.pid()).expect("attach");
        backend
            .resume(&ferros_core::backend::ResumeRequest::continue_all())
            .expect("resume");
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &target.pid().0.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !probe.has_pending_stop() {
            assert!(Instant::now() < deadline, "signal never reached the exception loop");
            std::thread::yield_now();
        }
        let began = Instant::now();
        drop(backend);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "drop waited for private SIGSTOP timeout"
        );
        // The old held signal is discarded, but the released program must run and
        // handle a fresh signal with the restored exception ports.
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &target.pid().0.to_string()])
                .status()
                .unwrap()
                .success()
        );
        observer.wait_released_after_drop(target.pid());
    });
}

#[test]
fn dropping_a_running_session_with_a_pending_stop_releases_the_target_promptly()
{
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let backend = MacOSBackend::new().expect("backend");
        let probe = backend.reply_fault_injector();
        let mut session = ferros_core::Session::new(Box::new(backend));
        session.attach(target.pid()).expect("attach");
        session.resume().expect("resume");
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &target.pid().0.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !probe.has_pending_stop() {
            assert!(Instant::now() < deadline, "signal never reached the exception loop");
            std::thread::yield_now();
        }
        let began = Instant::now();
        drop(session);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "session drop waited for private SIGSTOP timeout"
        );
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &target.pid().0.to_string()])
                .status()
                .unwrap()
                .success()
        );
        observer.wait_released_after_drop(target.pid());
    });
}

/// Two simultaneous logical signal decisions, with one retained across a
/// selective resume, must not share Darwin's process-wide ptrace signal state.
#[test]
fn only_the_selected_thread_receives_its_held_signal()
{
    use ferros_core::StopEvent;
    use ferros_core::backend::{ResumeRequest, ThreadAction};
    harness::run(|t| {
        for first in [0, 1] {
            for selected in [0, 1] {
                let observer = Observer::new();
                let target = t.spawn(&fixture!("thread_signals"), &[observer.0.to_str().unwrap()]);
                observer.wait("ready-0");
                observer.wait("ready-1");
                let mut backend = MacOSBackend::new().expect("backend");
                backend.attach(target.pid()).expect("attach");
                std::fs::write(observer.0.join(format!("signal-{first}")), b"go").unwrap();
                backend.resume(&ResumeRequest::continue_all()).expect("first resume");
                let first_thread = match backend.wait(Some(Duration::from_secs(2))).expect("first signal") {
                    Some(StopEvent::Signal { thread, signal: 30 }) => thread,
                    other => panic!("expected first SIGUSR1, got {other:?}"),
                };
                let second = 1 - first;
                std::fs::write(observer.0.join(format!("signal-{second}")), b"go").unwrap();
                let mut keep_first = ResumeRequest::continue_all();
                keep_first.thread_actions.push((first_thread, ThreadAction::Suspend));
                backend.resume(&keep_first).expect("keep first signal, run second worker");
                let second_thread = match backend.wait(Some(Duration::from_secs(2))).expect("second signal") {
                    Some(StopEvent::Signal { thread, signal: 30 }) => thread,
                    other => panic!("expected second SIGUSR1, got {other:?}"),
                };
                assert_ne!(first_thread, second_thread);
                assert_eq!(backend.wait(Some(Duration::ZERO)).expect("drain"), None);
                let selected_thread = if selected == first { first_thread } else { second_thread };
                backend
                    .resume(&ResumeRequest::continue_all().with_signal(selected_thread, 30))
                    .expect("deliver only the selected signal");
                observer.wait("done-0");
                observer.wait("done-1");
                observer.wait(&format!("handled-{selected}"));
                std::fs::write(observer.0.join("snapshot"), b"go").unwrap();
                observer.wait("observed");
                assert_eq!(
                    std::fs::read_to_string(observer.0.join("observed")).unwrap(),
                    (1 << selected).to_string()
                );
                assert!(!observer.0.join(format!("handled-{}", 1 - selected)).exists());
                backend.kill().expect("kill");
            }
        }
    });
}

/// An exception may have been received before `task_suspend`, but not published
/// until between resume's lock sections. Suspension alone does not exclude it.
#[test]
fn a_late_stop_during_resume_retains_its_signal_decision()
{
    use ferros_core::StopEvent;
    use ferros_core::backend::{BackendError, ResumeRequest, ThreadAction};
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("thread_signals"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready-0");
        observer.wait("ready-1");
        let mut backend = MacOSBackend::new().expect("backend");
        let probe = backend.reply_fault_injector();
        backend.attach(target.pid()).expect("attach");
        std::fs::write(observer.0.join("signal-0"), b"go").unwrap();
        backend.resume(&ResumeRequest::continue_all()).expect("first resume");
        let first = match backend.wait(Some(Duration::from_secs(2))).expect("first signal") {
            Some(StopEvent::Signal { thread, signal: 30 }) => thread,
            other => panic!("expected first SIGUSR1, got {other:?}"),
        };
        probe.pause_next_stop_publication();
        std::fs::write(observer.0.join("signal-1"), b"go").unwrap();
        let mut keep_first = ResumeRequest::continue_all();
        keep_first.thread_actions.push((first, ThreadAction::Suspend));
        backend.resume(&keep_first).expect("retain first signal");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !probe.stop_publication_is_paused() {
            assert!(Instant::now() < deadline, "second signal not received");
            std::thread::yield_now();
        }
        backend.interrupt().expect("stop before exception published");
        assert!(matches!(
            backend.wait(Some(Duration::from_secs(2))).expect("interrupt"),
            Some(StopEvent::Interrupted { .. })
        ));
        probe.publish_paused_stop_during_next_resume();
        assert!(
            matches!(
                backend.resume(&ResumeRequest::continue_all()),
                Err(BackendError::InvalidState(_))
            ),
            "resume must not answer a newly published stop without its decision"
        );
        let second = match backend.wait(Some(Duration::from_secs(2))).expect("late signal") {
            Some(StopEvent::Signal { thread, signal: 30 }) => thread,
            other => panic!("expected second SIGUSR1, got {other:?}"),
        };
        backend
            .resume(&ResumeRequest::continue_all().with_signal(second, 30))
            .expect("deliver late signal");
        observer.wait("handled-1");
        observer.wait("done-0");
        observer.wait("done-1");
        assert!(!observer.0.join("handled-0").exists());
        backend.kill().expect("kill");
    });
}
