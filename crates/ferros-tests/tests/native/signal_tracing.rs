//! Private ptrace handshake races (#57).
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferros_core::backend::Backend;
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness, scratch};

struct Observer(PathBuf);
impl Observer
{
    fn new() -> Self
    {
        Self(scratch::create_dir("ferros-signals"))
    }
    fn wait_released(&self, pid: ferros_core::ProcessId)
    {
        let progress_before = std::fs::read_to_string(self.0.join("progress"));
        let began = Instant::now();
        // Handler acknowledgements alone can precede a late stop. Observe five
        // subsequent progress advances too, within the same observation deadline.
        let mut last_progress = None;
        let mut advances = 0;
        loop {
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
            if self.0.join("handled").exists() && self.0.join("continued").exists() {
                let progress = std::fs::read_to_string(self.0.join("progress"))
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok());
                if let Some(progress) = progress {
                    if last_progress.is_some_and(|last| progress > last) {
                        advances += 1;
                        if advances == 5 {
                            return;
                        }
                    }
                    last_progress = Some(progress);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
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

/// A fallback `PT_DETACH` must still account for its resume, preserve racing
/// application signals, and continue the private job-control stop (#76).
#[test]
fn fallback_detach_releases_its_private_stop_and_preserves_racing_signals()
{
    use ferros_core::backend::BackendError;
    harness::run(|t| {
        for separate in [true, false] {
            let observer = Observer::new();
            let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
            observer.wait("ready");
            let mut backend = MacOSBackend::new().expect("backend");
            backend.attach(target.pid()).expect("attach");
            let probe = backend.reply_fault_injector();
            probe.race_next_private_stop(separate);
            probe.fail_next_detach_handshake();
            assert!(matches!(backend.detach(), Err(BackendError::DetachCleanup(_))));
            // No rescue signal: both handler acknowledgements must come from
            // fallback cleanup and the signal racing its private stop.
            observer.wait_released(target.pid());
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
        observer.wait_released(target.pid());
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
        let mut session = ferros_macos::session_with(backend);
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
        observer.wait_released(target.pid());
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
            matches!(backend.resume(&ResumeRequest::continue_all()), Err(BackendError::StopPending)),
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

// Native negative cases for signal requests and resume rollback (#290, review F18, F19).
// `MockBackend` checks pending stops before it validates a request and never touches a
// suspend count or a step bit, so only these tests see the native order and rollback.

/// `SIGUSR1` on macOS.
const SIGUSR1: i32 = 30;

/// The fixture's progress counter, as written. The main thread rewrites it every
/// millisecond while it runs.
fn progress(observer: &Observer) -> Option<String>
{
    std::fs::read_to_string(observer.0.join("progress")).ok()
}

/// The target's main loop makes no progress, so no thread was released.
fn assert_stays_stopped(observer: &Observer, context: &str)
{
    let before = progress(observer);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(progress(observer), before, "the target ran after {context}");
}

/// The target's main loop advances again.
fn assert_runs(observer: &Observer)
{
    let before = progress(observer);
    let deadline = Instant::now() + Duration::from_secs(2);
    while progress(observer) == before {
        assert!(Instant::now() < deadline, "the target did not run after resume");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A refused request is `Unsupported` with a message naming the failed check.
fn assert_refused(result: ferros_core::backend::BackendResult<()>, needle: &str, context: &str)
{
    match result {
        Err(ferros_core::backend::BackendError::Unsupported(message)) => {
            assert!(message.contains(needle), "{context}: refused for another reason: {message}");
        }
        other => panic!("{context}: expected Unsupported ({needle}), got {other:?}"),
    }
}

/// F18 (a): with no held signal at all, a resume naming a signal is refused before
/// anything runs. The target stays stopped and a plain resume still works.
#[test]
fn resume_with_a_signal_but_no_held_signal_is_refused()
{
    use ferros_core::backend::ResumeRequest;
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let mut backend = MacOSBackend::new().expect("backend");
        backend.attach(target.pid()).expect("attach");
        let threads = backend.threads().expect("threads");
        assert!(threads.len() >= 2, "expected the main thread and the worker: {threads:?}");
        for &thread in &threads {
            for signal in [SIGUSR1, 0] {
                assert_refused(
                    backend.resume(&ResumeRequest::continue_all().with_signal(thread, signal)),
                    "held soft-signal",
                    &format!("signal {signal} for thread {thread:?} with none held"),
                );
                assert_stays_stopped(&observer, "a refused signal resume");
            }
        }
        assert_eq!(backend.wait(Some(Duration::ZERO)).expect("no event"), None);
        backend.resume(&ResumeRequest::continue_all()).expect("plain resume");
        assert_runs(&observer);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!observer.0.join("handled").exists(), "a refused request delivered a signal");
        backend.kill().expect("kill");
    });
}

/// F18 (b): a held signal cannot be replaced by a number outside `0..32`. Each refusal
/// leaves the target stopped and the signal held, and the held signal is then delivered.
#[test]
fn out_of_range_replacement_signal_is_refused_and_keeps_the_held_signal()
{
    use ferros_core::StopEvent;
    use ferros_core::backend::ResumeRequest;
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let mut backend = MacOSBackend::new().expect("backend");
        backend.attach(target.pid()).expect("attach");
        backend.resume(&ResumeRequest::continue_all()).expect("resume");
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-USR1", &target.pid().0.to_string()])
                .status()
                .expect("send SIGUSR1")
                .success()
        );
        let thread = match backend.wait(Some(Duration::from_secs(2))).expect("signal stop") {
            Some(StopEvent::Signal { thread, signal: SIGUSR1 }) => thread,
            other => panic!("expected SIGUSR1, got {other:?}"),
        };
        for signal in [32, 33, -1, i32::MAX, i32::MIN] {
            assert_refused(
                backend.resume(&ResumeRequest::continue_all().with_signal(thread, signal)),
                "invalid signal",
                &format!("replacement signal {signal}"),
            );
            assert_stays_stopped(&observer, "a refused replacement signal");
        }
        assert_eq!(backend.wait(Some(Duration::ZERO)).expect("no event"), None);
        assert!(!observer.0.join("handled").exists(), "the held signal was delivered early");
        backend
            .resume(&ResumeRequest::continue_all().with_signal(thread, SIGUSR1))
            .expect("deliver the held signal");
        observer.wait("handled");
        backend.kill().expect("kill");
    });
}

#[test]
fn signal_for_a_thread_kept_suspended_is_refused()
{
    use ferros_core::StopEvent;
    use ferros_core::backend::{ResumeRequest, ThreadAction};
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("thread_signals"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready-0");
        observer.wait("ready-1");
        let mut backend = MacOSBackend::new().expect("backend");
        backend.attach(target.pid()).expect("attach");
        std::fs::write(observer.0.join("signal-0"), b"go").expect("open worker signal gate");
        backend.resume(&ResumeRequest::continue_all()).expect("run to signal");
        let thread = match backend.wait(Some(Duration::from_secs(2))).expect("signal stop") {
            Some(StopEvent::Signal { thread, signal: SIGUSR1 }) => thread,
            other => panic!("expected worker SIGUSR1, got {other:?}"),
        };

        let deliver = ResumeRequest::continue_all().with_signal(thread, SIGUSR1);
        let suspend = deliver.clone().with_thread(thread, ThreadAction::Suspend);
        assert_refused(backend.resume(&suspend), "stays suspended", "signal for suspended worker");
        assert!(!observer.0.join("handled-0").exists(), "the held signal was delivered early");
        backend.resume(&deliver).expect("deliver the held signal");
        observer.wait("handled-0");
        backend.kill().expect("kill");
    });
}

/// F18 (c): native resume validates a request before it looks at queued stops, unlike
/// `MockBackend`. A real step completes in the same exception batch as a real SIGUSR1 on
/// a peer, so the signal stop is queued behind the step's. Invalid requests are refused
/// as invalid, not as `StopPending`; the queued stop and the target are left as they were.
#[cfg(target_arch = "aarch64")]
#[test]
fn invalid_signal_request_with_a_stop_queued_is_refused_before_the_queue_is_checked()
{
    use ferros_core::StopEvent;
    use ferros_core::backend::ResumeRequest;
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let mut backend = MacOSBackend::new().expect("backend");
        let probe = backend.reply_fault_injector();
        backend.attach(target.pid()).expect("attach");
        let threads = backend.threads().expect("threads");
        assert!(threads.len() >= 2, "expected the main thread and the worker: {threads:?}");
        let (stepped, peer) = (threads[0], threads[1]);
        // The step request suspends the peer once; the hook delivers SIGUSR1 to it and
        // batches that real signal with the real step completion.
        probe.batch_next_step_with_signal(peer);
        backend.resume(&ResumeRequest::step(stepped)).expect("step");
        assert_eq!(
            backend.wait(Some(Duration::from_secs(5))).expect("step stop"),
            Some(StopEvent::SingleStep { thread: stepped })
        );
        // The peer's signal is held and queued, not yet returned by `wait`.
        let invalid = vec![
            (
                ResumeRequest::continue_all().with_signal(stepped, SIGUSR1),
                "held soft-signal",
                "a signal for the stepped thread, which holds none",
            ),
            (
                ResumeRequest::continue_all().with_signal(peer, 32),
                "invalid signal",
                "an out-of-range replacement for the queued signal",
            ),
            (
                ResumeRequest::continue_all().with_signal(peer, -1),
                "invalid signal",
                "a negative replacement for the queued signal",
            ),
        ];
        for (request, needle, context) in invalid {
            assert_refused(backend.resume(&request), needle, context);
            assert_stays_stopped(&observer, context);
        }
        assert_eq!(
            backend.wait(Some(Duration::ZERO)).expect("queued stop"),
            Some(StopEvent::Signal {
                thread: peer,
                signal: SIGUSR1
            }),
            "the refusals changed the queued stop"
        );
        assert_eq!(backend.wait(Some(Duration::ZERO)).expect("drained"), None);
        assert!(!observer.0.join("handled").exists(), "the held signal was delivered early");
        backend
            .resume(&ResumeRequest::continue_all().with_signal(peer, SIGUSR1))
            .expect("deliver the held signal");
        observer.wait("handled");
        backend.kill().expect("kill");
    });
}

/// F19 (d): a resume refused after it suspended and armed threads (here `task_resume`
/// fails) gives back every per-thread suspend and clears every step bit it set, before
/// it returns. A retry then steps normally.
#[cfg(target_arch = "aarch64")]
#[test]
fn refused_step_or_suspend_resume_rolls_back_suspends_and_step_bits()
{
    use ferros_core::StopEvent;
    use ferros_core::backend::{ResumeRequest, ThreadAction};
    use ferros_macos::diagnostics;
    harness::run(|t| {
        let observer = Observer::new();
        let target = t.spawn(&fixture!("signal_tracing"), &[observer.0.to_str().unwrap()]);
        observer.wait("ready");
        let mut backend = MacOSBackend::new().expect("backend");
        let probe = backend.reply_fault_injector();
        backend.attach(target.pid()).expect("attach");
        let threads = backend.threads().expect("threads");
        assert!(threads.len() >= 2, "expected the main thread and the worker: {threads:?}");
        let (stepped, peer) = (threads[0], threads[1]);
        let counts = |threads: &[ferros_core::ThreadId]| -> Vec<i32> {
            threads
                .iter()
                .map(|&thread| diagnostics::thread_suspend_count(thread).expect("suspend count"))
                .collect()
        };
        let baseline = counts(&threads);
        for &thread in &threads {
            assert!(
                !diagnostics::single_step_enabled(thread).expect("MDSCR_EL1.SS"),
                "thread {thread:?} had a step bit before any step"
            );
        }
        let requests = vec![
            (ResumeRequest::step(stepped), "a step of one thread"),
            (
                ResumeRequest::continue_all().with_thread(peer, ThreadAction::Suspend),
                "a continue that keeps one thread suspended",
            ),
            (
                ResumeRequest::continue_all()
                    .with_thread(stepped, ThreadAction::Step)
                    .with_thread(peer, ThreadAction::Suspend),
                "a step with one other thread suspended",
            ),
        ];
        for (request, context) in requests {
            probe.fail_next_task_resume();
            let refused = backend.resume(&request).expect_err(context);
            assert!(refused.to_string().contains("task_resume"), "{context}: {refused}");
            assert_eq!(counts(&threads), baseline, "{context}: per-thread suspends kept");
            for &thread in &threads {
                assert!(
                    !diagnostics::single_step_enabled(thread).expect("MDSCR_EL1.SS"),
                    "{context}: thread {thread:?} kept its step bit"
                );
            }
            assert_stays_stopped(&observer, context);
            assert_eq!(backend.wait(Some(Duration::ZERO)).expect("no event"), None, "{context}");
        }
        backend.resume(&ResumeRequest::step(stepped)).expect("retry the step");
        assert_eq!(
            backend.wait(Some(Duration::from_secs(5))).expect("step stop"),
            Some(StopEvent::SingleStep { thread: stepped })
        );
        backend.kill().expect("kill");
    });
}
