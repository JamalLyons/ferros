//! Lifecycle tests: `signals`.

use std::fs;
use std::time::{Duration, Instant};

use ferros_core::backend::ResumeRequest;
use ferros_core::{Session, SessionState, StopEvent};
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness, scratch};

use super::{SIGUSR1, TIMEOUT, assert_reaped, process_state, signal};

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

/// An early-replied signal still supports both suppression and delivery to a
/// real handler; the handler cannot run before the client's resume decision.
#[test]
fn early_replied_signal_preserves_handler_delivery_and_suppression()
{
    harness::run(|t| {
        for deliver in [false, true] {
            let mut d = t.launch(&fixture!("signal_handler"));
            d.resume().expect("resume");
            assert!(d.wait_for_stdout("ready", TIMEOUT));
            signal(d.pid().0, "-CONT");
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("SIGCONT stop"),
                Some(StopEvent::Signal { signal: 19, .. })
            ));
            assert!(
                !d.wait_for_stdout("continued", Duration::from_millis(150)),
                "handler ran before resume"
            );
            if !deliver {
                d.resume().expect("suppress SIGCONT");
                assert!(
                    !d.wait_for_stdout("continued", Duration::from_millis(150)),
                    "suppressed handler ran"
                );
                signal(d.pid().0, "-CONT");
                assert!(matches!(
                    d.wait(Some(TIMEOUT)).expect("fresh SIGCONT stop"),
                    Some(StopEvent::Signal { signal: 19, .. })
                ));
            }
            d.resume_with_signal().expect("deliver SIGCONT");
            assert!(d.wait_for_stdout("continued", TIMEOUT), "selected handler did not run");
            assert_eq!(
                d.wait(Some(Duration::from_millis(150))).expect("no duplicate signal stop"),
                None
            );
            let pid = d.pid().0;
            d.kill().expect("kill");
            assert_reaped(pid);
        }
    });
}

#[test]
fn early_replied_signal_can_be_replaced_with_sigkill()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let pid = d.pid().0;
        signal(pid, "-USR1");
        d.resume().expect("resume to SIGUSR1");
        let thread = match d.wait(Some(TIMEOUT)).expect("signal stop") {
            Some(StopEvent::Signal { signal: SIGUSR1, thread }) => thread,
            other => panic!("expected SIGUSR1, got {other:?}"),
        };
        d.backend_mut()
            .resume(&ResumeRequest::continue_all().with_signal(thread, 9))
            .expect("select SIGKILL");
        assert_eq!(
            d.wait(Some(TIMEOUT)).expect("terminal event"),
            Some(StopEvent::Killed { signal: 9 })
        );
        assert_reaped(pid);
    });
}

/// Removes a `signal_tracing` acknowledgement directory on unwind.
struct SignalObserver(std::path::PathBuf);

impl SignalObserver
{
    fn new() -> Self
    {
        Self(scratch::create_dir("ferros-lifecycle-signals"))
    }

    fn arg(&self) -> &str
    {
        self.0.to_str().expect("observer path must be UTF-8")
    }

    fn acknowledged(&self, file: &str) -> bool
    {
        self.0.join(file).exists()
    }

    fn progress(&self) -> Option<u64>
    {
        fs::read_to_string(self.0.join("progress")).ok()?.parse().ok()
    }

    fn wait_for(&self, file: &str)
    {
        let deadline = Instant::now() + TIMEOUT;
        while !self.acknowledged(file) {
            assert!(Instant::now() < deadline, "missing {file} acknowledgement");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait for the fixture's main loop to write `advances` more progress values, so
    /// every signal handled before this call has had its acknowledgement written.
    fn wait_for_progress(&self, advances: u64)
    {
        let deadline = Instant::now() + TIMEOUT;
        let start = loop {
            if let Some(progress) = self.progress() {
                break progress;
            }
            assert!(Instant::now() < deadline, "no progress acknowledgement");
            std::thread::sleep(Duration::from_millis(1));
        };
        while self.progress().is_none_or(|progress| progress < start + advances) {
            assert!(Instant::now() < deadline, "the fixture stopped making progress");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for SignalObserver
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A nonfatal replacement is passed once, without exposing a second signal stop: the
/// target's SIGCONT handler runs, and its SIGUSR1 handler never does.
#[test]
fn held_signal_can_be_replaced_with_a_nonfatal_signal()
{
    harness::run(|t| {
        let observer = SignalObserver::new();
        let mut d = t.launch_with_args(&fixture!("signal_tracing"), &[observer.arg()]);
        let pid = d.pid().0;
        d.resume().expect("run handler setup");
        observer.wait_for("ready");
        // Neither handler has run yet, so each later acknowledgement is the replacement's.
        observer.wait_for_progress(3);
        assert!(!observer.acknowledged("continued"), "SIGCONT handler ran before any SIGCONT");
        assert!(!observer.acknowledged("handled"), "SIGUSR1 handler ran before any SIGUSR1");
        signal(pid, "-USR1");
        let thread = match d.wait(Some(TIMEOUT)).expect("signal stop") {
            Some(StopEvent::Signal { signal: SIGUSR1, thread }) => thread,
            other => panic!("expected SIGUSR1, got {other:?}"),
        };
        // Darwin SIGCONT is 19. Its default action is nonfatal.
        d.session_mut()
            .backend_mut()
            .resume(&ResumeRequest::continue_all().with_signal(thread, 19))
            .expect("replace SIGUSR1 with SIGCONT");
        observer.wait_for("continued");
        observer.wait_for_progress(5);
        assert!(!observer.acknowledged("handled"), "the replaced SIGUSR1 was delivered too");
        // A second delivery of SIGCONT would stop the traced target again.
        assert_eq!(d.wait(Some(Duration::from_millis(150))).expect("no duplicate stop"), None);
        d.kill().expect("kill");
        assert_reaped(pid);
    });
}

/// Each replacement decision consumes just its own internal signal stop.
#[test]
fn replacement_signals_do_not_hide_later_signal_stops()
{
    harness::run(|t| {
        // SIGCONT, SIGCHLD, SIGURG and SIGWINCH have nonfatal default actions.
        const CASES: &[(i32, &str)] = &[(19, "-CONT"), (20, "-CHLD"), (16, "-URG"), (28, "-WINCH")];
        for &(replacement, name) in CASES {
            let mut d = t.launch(&fixture!("loop_counter"));
            let pid = d.pid().0;
            signal(pid, "-USR1");
            d.resume().expect("resume to original signal");
            let thread = match d.wait(Some(TIMEOUT)).expect("original signal stop") {
                Some(StopEvent::Signal { signal: SIGUSR1, thread }) => thread,
                other => panic!("expected SIGUSR1, got {other:?}"),
            };
            d.backend_mut()
                .resume(&ResumeRequest::continue_all().with_signal(thread, replacement))
                .expect("replace with a nonfatal signal");
            assert_eq!(d.wait(Some(Duration::from_millis(100))).expect("no duplicate stop"), None);
            let stat = process_state(pid);
            assert!(!stat.starts_with('T'), "replacement left BSD state stopped: {stat}");
            signal(pid, name);
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("later signal stop"),
                Some(StopEvent::Signal { signal, .. }) if signal == replacement
            ));
            d.resume().expect("suppress later signal");
            d.kill().expect("kill");
            assert_reaped(pid);
        }
    });
}

/// #79: a signal sent while the backend holds the target stopped at entry, after
/// attach, or at an interrupt, is not published until resume. Published earlier, it
/// raced the client's resume, which had already drained the queue, and the resume
/// was refused with stop events pending. Delivery and suppression still apply to it.
#[test]
fn signal_sent_while_stopped_is_reported_after_resume()
{
    harness::run(|t| {
        const CASES: &[(&str, bool)] = &[("entry", false), ("entry", true), ("attach", false), ("interrupt", true)];
        for &(stop, deliver) in CASES {
            let spawned = (stop == "attach").then(|| t.spawn(&fixture!("loop_counter"), &[]));
            let mut d = match &spawned {
                Some(target) => {
                    let mut session = ferros_macos::new_session().expect("backend");
                    session.attach(target.pid()).expect("attach");
                    Err(session)
                }
                None => Ok(t.launch(&fixture!("loop_counter"))),
            };
            let session: &mut Session = match &mut d {
                Ok(debuggee) => debuggee,
                Err(session) => session,
            };
            let pid = session.pid().expect("pid").0;
            if stop == "interrupt" {
                session.resume().expect("resume before interrupt");
                session.interrupt().expect("interrupt");
                assert!(matches!(
                    session.wait(Some(TIMEOUT)).expect("interrupt stop"),
                    Some(StopEvent::Interrupted { .. })
                ));
            }
            signal(pid, "-USR1");
            assert_eq!(
                session
                    .backend_mut()
                    .wait(Some(Duration::from_millis(300)))
                    .expect("wait while stopped"),
                None,
                "{stop}: a signal sent while stopped was published before resume"
            );
            session
                .backend_mut()
                .resume(&ResumeRequest::continue_all())
                .unwrap_or_else(|err| panic!("{stop}: resume refused: {err:?}"));
            let thread = match session.backend_mut().wait(Some(TIMEOUT)).expect("signal stop") {
                Some(StopEvent::Signal { signal: SIGUSR1, thread }) => thread,
                other => panic!("{stop}: expected SIGUSR1, got {other:?}"),
            };
            assert_eq!(session.backend_mut().wait(Some(Duration::ZERO)).expect("drain"), None);
            if deliver {
                session
                    .backend_mut()
                    .resume(&ResumeRequest::continue_all().with_signal(thread, SIGUSR1))
                    .expect("deliver SIGUSR1");
                assert_eq!(
                    session.backend_mut().wait(Some(TIMEOUT)).expect("terminal event"),
                    Some(StopEvent::Killed { signal: SIGUSR1 }),
                    "{stop}: delivered signal"
                );
            } else {
                session
                    .backend_mut()
                    .resume(&ResumeRequest::continue_all())
                    .expect("suppress SIGUSR1");
                assert_eq!(
                    session
                        .backend_mut()
                        .wait(Some(Duration::from_millis(150)))
                        .expect("no duplicate stop"),
                    None,
                    "{stop}: suppressed signal stopped again"
                );
                assert!(!harness::process_is_gone(pid), "{stop}: suppressed signal killed the target");
                session.kill().expect("kill");
            }
            assert_reaped(pid);
        }
    });
}

/// #79: a signal deferred while stopped is torn down with the session. Kill reaps
/// the target; detach discards the client decision and lets the process run on, and
/// `Session::resume` reports the deferred signal as its next stop.
#[test]
fn signal_deferred_while_stopped_survives_teardown_and_session_resume()
{
    harness::run(|t| {
        for ending in ["kill", "detach", "session resume"] {
            let target = t.spawn(&fixture!("loop_counter"), &["100000"]);
            let pid = target.pid();
            let backend = MacOSBackend::new().expect("backend");
            let faults = backend.reply_fault_injector();
            let mut session = ferros_macos::session_with(backend);
            session.attach(pid).expect("attach");
            signal(pid.0, "-USR1");
            assert_eq!(
                session
                    .backend_mut()
                    .wait(Some(Duration::from_millis(300)))
                    .expect("wait while stopped"),
                None,
                "{ending}: a signal sent while stopped was published before resume"
            );
            assert!(!faults.has_pending_stop(), "{ending}: deferral reported as a stop");
            match ending {
                "kill" => {
                    session.kill().expect("kill with a deferred signal");
                    assert_reaped(pid.0);
                }
                "detach" => {
                    session.detach().expect("detach with a deferred signal");
                    std::thread::sleep(Duration::from_millis(200));
                    assert!(!harness::process_is_gone(pid.0), "detach delivered the deferred signal");
                    let stat = process_state(pid.0);
                    assert!(!stat.starts_with('T'), "detach left the target stopped: {stat}");
                }
                _ => {
                    session.resume().expect("session resume with a deferred signal");
                    assert!(matches!(
                        session.wait(Some(TIMEOUT)).expect("signal stop"),
                        Some(StopEvent::Signal { signal: SIGUSR1, .. })
                    ));
                    session.kill().expect("kill");
                    assert_reaped(pid.0);
                }
            }
        }
    });
}

/// Replacement must also refresh the properties of a nonfatal original signal.
#[test]
fn held_nonfatal_signal_can_be_replaced_with_a_fatal_signal()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let pid = d.pid().0;
        signal(pid, "-CONT");
        d.resume().expect("resume to SIGCONT");
        let thread = match d.wait(Some(TIMEOUT)).expect("signal stop") {
            Some(StopEvent::Signal { signal: 19, thread }) => thread,
            other => panic!("expected SIGCONT, got {other:?}"),
        };
        d.backend_mut()
            .resume(&ResumeRequest::continue_all().with_signal(thread, SIGUSR1))
            .expect("replace SIGCONT with SIGUSR1");
        assert_eq!(
            d.wait(Some(TIMEOUT)).expect("signal exit without duplicate stop"),
            Some(StopEvent::Killed { signal: SIGUSR1 })
        );
        assert_reaped(pid);
    });
}
