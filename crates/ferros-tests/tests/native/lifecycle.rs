//! Process lifecycle through `ferros_core::Session` on the macOS backend.
#![cfg(target_os = "macos")]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Command;
use std::time::{Duration, Instant};

use ferros_core::arch::Arm64;
use ferros_core::backend::{Backend, BackendError, ResumeRequest};
use ferros_core::{BreakpointLocation, LaunchSpec, ProcessId, ProcessOwnership, Session, SessionState, StopEvent};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness, scratch};

const TIMEOUT: Duration = Duration::from_secs(5);
// Darwin signal numbers; the test harness deliberately uses no raw FFI.
const SIGUSR1: i32 = 30;
const SIGKILL: i32 = 9;

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

/// The `ps` state of `pid` (for example `S`, `R` or `T`), empty if it does not exist.
fn process_state(pid: u32) -> String
{
    let output = Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Removes captured output or a fixture exit gate on unwind.
struct ReleaseFile(std::path::PathBuf);

impl ReleaseFile
{
    fn exit_gate(t: &harness::TestContext) -> Self
    {
        // Registered with the harness too: `Drop` does not run when the deadline fires.
        let (_, path) = t.scratch_file("ferros-release", ".gate");
        Self(path)
    }

    fn gate_args(&self) -> [&str; 3]
    {
        ["0", "--exit-gate", self.0.to_str().expect("exit gate path must be UTF-8")]
    }

    fn open_gate(&self)
    {
        fs::write(&self.0, b"exit\n").expect("open exit gate");
    }
}

impl Drop for ReleaseFile
{
    fn drop(&mut self)
    {
        let _ = fs::remove_file(&self.0);
    }
}

/// A fixture started through `/bin/sh`, so its parent is the shell and not this test
/// process: attaching reparents it to the debugger and detaching hands it back, as for a
/// process started from a terminal. The shell waits for it, so the shell exits when the
/// fixture does.
struct Grandchild
{
    /// Reserved first so every process carrying it dies with this value, and on the test's
    /// deadline even if this value is never dropped.
    tag: harness::Tag,
    shell: std::process::Child,
    pid: u32,
    output_path: ReleaseFile,
    exit_gate: ReleaseFile,
}

impl Grandchild
{
    fn spawn(t: &harness::TestContext) -> Self
    {
        let tag = t.tag();
        let exit_gate = ReleaseFile::exit_gate(t);
        // Files cannot fill a pipe and delay the very completion being measured.
        let (output, output_path) = t.scratch_file("ferros-release", ".log");
        let output_path = ReleaseFile(output_path);
        let shell = Command::new("/bin/sh")
            .arg("-c")
            // Not just `"$@"`: the shell would exec the fixture instead of waiting for it.
            .arg("\"$@\"; status=$?; exit $status")
            .arg("sh")
            .arg(fixture!("loop_counter").path())
            .args(exit_gate.gate_args())
            // Both the shell and the fixture carry the tag in their argument lists.
            .arg(tag.as_str())
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone().expect("clone release output"))
            .stderr(output)
            .spawn()
            .expect("spawn /bin/sh");
        // Install cleanup before PID discovery can fail. Zero means undiscovered;
        // Drop then finds children by this shell's PID rather than signalling PID 0.
        let mut target = Self {
            tag,
            shell,
            pid: 0,
            output_path,
            exit_gate,
        };
        let deadline = Instant::now() + TIMEOUT;
        let pid = loop {
            let output = Command::new("/usr/bin/pgrep")
                .args(["-P", &target.shell.id().to_string()])
                .output()
                .expect("pgrep");
            if let Some(pid) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the shell did not start the fixture");
            std::thread::sleep(Duration::from_millis(10));
        };
        target.pid = pid;
        target
    }

    /// Capture both processes before Drop kills them. A live fixture with `T` or
    /// nonzero Mach counts differs from a zombie or a parent that has not reaped it.
    fn diagnostics(&self) -> String
    {
        let ps = Command::new("/bin/ps")
            .args(["-o", "pid=,ppid=,stat=,etime=,time=,wchan=,command=", "-p"])
            .arg(format!("{},{}", self.pid, self.shell.id()))
            .output()
            .expect("release diagnostic ps");
        let mut snapshot = format!("ps status={}:\n{}", ps.status, String::from_utf8_lossy(&ps.stdout));
        write!(
            snapshot,
            "\nMach snapshot: {:?}\nexception ports: {:?}\nfixture output: {}",
            diagnostics::process_suspend_counts(ProcessId::from(self.pid)),
            diagnostics::exception_ports(ProcessId::from(self.pid)),
            fs::read_to_string(&self.output_path.0).unwrap_or_else(|error| error.to_string()),
        )
        .expect("write diagnostic string");
        snapshot
    }

    /// Sample across the release window while the fixture is still gated. `ps` state
    /// is reliable for this target because it was not launched suspended.
    fn assert_not_stopped(&self, case: &str)
    {
        for sample in 0..5 {
            if sample != 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            let stat = process_state(self.pid);
            assert!(
                !stat.starts_with('T'),
                "{case}: the released process is stopped (state {stat}); {}",
                self.diagnostics()
            );
        }
    }

    /// Ask the fixture to exit and wait for the shell to return its status.
    fn open_gate_and_wait(&mut self) -> Option<std::process::ExitStatus>
    {
        self.exit_gate.open_gate();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self
                .shell
                .try_wait()
                .unwrap_or_else(|error| panic!("wait for release parent: {error}; {}", self.diagnostics()))
            {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Grandchild
{
    fn drop(&mut self)
    {
        // Kill by tag, never by pid: once `open_gate_and_wait` has reaped the shell, and the
        // shell the fixture, both pids may belong to unrelated processes. The tag matches
        // only live processes that still carry it.
        let _ = harness::kill_tagged(self.tag.as_str());
        // `Child::kill` and `wait` never signal a pid this `Child` already reaped.
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}

/// A process the debugger releases runs on to completion: it is not left stopped by the
/// private SIGSTOP that ends tracing, and a user signal held when it is released is
/// discarded, not delivered.
///
/// The released process must not be in state `T`, and `loop_counter` must exit 0 when
/// the test opens its exit gate (a delivered SIGUSR1 would kill it). The
/// target's parent is a shell, as for a process started from a terminal (see
/// [`Grandchild`]).
#[test]
fn released_processes_run_to_completion()
{
    /// How the attached process is released.
    const CASES: &[&str] = &[
        "detach after inspecting",
        "detach",
        "drop stopped",
        "drop running",
        "backend drop",
        "detach with held signal",
    ];

    harness::run(|t| {
        // A released fixture can be checked while the next case attaches its own target.
        std::thread::scope(|scope| {
            for &case in CASES {
                let mut target = Grandchild::spawn(t);
                let pid = ProcessId::from(target.pid);
                let mut session = ferros_macos::new_session().expect("backend");
                let started = Instant::now();
                session.attach(pid).expect("attach");
                match case {
                    "detach" => session.detach().expect("detach"),
                    "detach after inspecting" => {
                        // What `ferros attach --headless` does before it detaches.
                        session.threads().expect("threads");
                        session.memory_regions().expect("memory_regions");
                        session.detach().expect("detach");
                    }
                    "drop stopped" => drop(session),
                    "drop running" => {
                        session.resume().expect("resume");
                        drop(session);
                    }
                    "backend drop" => {
                        session.detach().expect("detach the session");
                        let mut backend = MacOSBackend::new().expect("backend");
                        backend.attach(pid).expect("attach the bare backend");
                        drop(backend);
                    }
                    "detach with held signal" => {
                        session.resume().expect("resume");
                        signal(target.pid, "-USR1");
                        assert!(matches!(
                            session.wait(Some(TIMEOUT)).expect("signal stop"),
                            Some(StopEvent::Signal { signal: SIGUSR1, .. })
                        ));
                        // Detach discards the held signal (SIGUSR1 would terminate it).
                        session.detach().expect("detach with a held signal");
                    }
                    _ => unreachable!(),
                }
                let released = Instant::now();
                eprintln!("{case}: attach/release took {:?}", released.duration_since(started));
                scope.spawn(move || {
                    target.assert_not_stopped(case);
                    let status = target.open_gate_and_wait().unwrap_or_else(|| {
                        panic!(
                            "{case}: no completion after {:?}; {}",
                            released.elapsed(),
                            target.diagnostics()
                        )
                    });
                    eprintln!("{case}: completed {:?} after release", released.elapsed());
                    assert!(
                        status.success(),
                        "{case}: the released process exited with {status}; {}",
                        target.diagnostics()
                    );
                });
            }
        });

        // A launched process released with detach also runs to completion.
        let exit_gate = ReleaseFile::exit_gate(t);
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &exit_gate.gate_args());
        d.detach().expect("detach a launched process");
        exit_gate.open_gate();
        assert!(
            d.wait_for_stdout("final count: 0", TIMEOUT),
            "the detached launched process did not run to completion: state/Mach={:?}, stdout/stderr={:?}",
            (process_state(d.pid().0), diagnostics::process_suspend_counts(d.pid())),
            (d.stdout(), d.stderr())
        );
    });
}

/// Other signals must never let user code run while the signal stop is held (#56).
/// A private SIGSTOP that takes effect after detach's wait gave up and sent its SIGCONT
/// (#149) is overtaken by nothing: detach continues it again, so the released process
/// is not left stopped and runs to completion.
#[test]
fn detach_continues_a_private_stop_that_lands_after_its_wait()
{
    harness::run(|t| {
        let mut target = Grandchild::spawn(t);
        let mut backend = MacOSBackend::new().expect("backend");
        backend.attach(ProcessId::from(target.pid)).expect("attach");
        backend
            .reply_fault_injector()
            // The recheck returns as soon as the stop lands; a long bound only adds margin.
            .expire_next_detach_stop_wait(TIMEOUT);
        backend.detach().expect("detach after a late private stop");
        target.assert_not_stopped("detach after a late private stop");
        let status = target
            .open_gate_and_wait()
            .unwrap_or_else(|| panic!("no completion after detach; {}", target.diagnostics()));
        assert!(status.success(), "the released process exited with {status}");
    });
}

/// If detach never sees its private SIGSTOP take effect, it reports that the released
/// process may stay stopped instead of returning `Ok` (#149).
#[test]
fn detach_reports_a_private_stop_it_never_saw()
{
    harness::run(|t| {
        let mut target = Grandchild::spawn(t);
        let mut backend = MacOSBackend::new().expect("backend");
        backend.attach(ProcessId::from(target.pid)).expect("attach");
        let real_stop = backend.reply_fault_injector().hide_next_detach_stop(TIMEOUT);
        let error = backend.detach().expect_err("an unseen private stop is reported");
        assert!(
            matches!(error, BackendError::DetachCleanup(ref message) if message.contains("may stay stopped")),
            "{error:?}"
        );
        assert_eq!(
            real_stop.try_recv(),
            Ok(true),
            "the real private stop must take effect before SIGCONT; {}",
            target.diagnostics()
        );
        let status = target
            .open_gate_and_wait()
            .unwrap_or_else(|| panic!("no completion after detach; {}", target.diagnostics()));
        assert!(status.success(), "the released process exited with {status}");
    });
}

#[test]
fn ordinary_signals_do_not_release_a_signal_stop()
{
    harness::run(|t| {
        for other in "-USR2 -TERM -CONT -STOP".split_whitespace() {
            let mut d = t.launch_with_args(&fixture!("loop_counter"), &["25", "--progress"]);
            let pid = d.pid().0;
            // Queue the signal before any fixture code runs.
            signal(pid, "-USR1");
            d.resume().expect("resume to the signal");
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("signal stop"),
                Some(StopEvent::Signal { signal: SIGUSR1, .. })
            ));
            signal(pid, other);
            assert!(
                !d.wait_for_stdout("tick:", Duration::from_millis(400)),
                "fixture ran after {other} during a signal stop"
            );
            assert!(!harness::process_is_gone(pid), "fixture died after {other}");
            assert_eq!(d.state(), SessionState::Stopped);
            signal(pid, "-9");
            assert_reaped(pid);
            assert_eq!(
                d.wait(Some(TIMEOUT)).expect("terminal event"),
                Some(StopEvent::Killed { signal: 9 })
            );
            assert_eq!(d.state(), SessionState::Exited);
        }
    });
}

/// #58 L-1: early replying and resuming fail independently. Each error is reported
/// once, and neither failure may leave the Session claiming to run.
#[test]
fn early_reply_and_task_resume_failures_are_reported_once_each()
{
    harness::run(|t| {
        let backend = MacOSBackend::new().expect("backend");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture!("loop_counter"), &["100000"], backend);
        d.resume().expect("resume");
        faults.fail_next_exception_reply();
        signal(d.pid().0, "-USR1");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("signal stop"),
            Some(StopEvent::Signal { signal: SIGUSR1, .. })
        ));
        faults.fail_next_task_resume();
        let first = d.resume().expect_err("queued early reply failure");
        assert!(first.to_string().contains("reply"), "{first}");
        assert_eq!(d.state(), SessionState::Stopped);
        let second = d.resume().expect_err("task_resume failure");
        assert!(second.to_string().contains("task_resume"), "{second}");
        assert_eq!(d.state(), SessionState::Stopped);
        d.resume().expect("both errors consumed; retry runs");
        d.interrupt().expect("interrupt after recovery");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("later stop"),
            Some(StopEvent::Interrupted { .. })
        ));
    });
}

/// #242: a failed `task_resume` leaves the signal held and unsent. A retry can deliver it
/// once or suppress it, and a later held signal still forwards.
#[test]
fn held_signal_survives_a_forwarding_resume_whose_task_resume_fails()
{
    harness::run(|t| {
        let fixture = fixture!("signal_handler");
        let file = format!("{}.rs", fixture.name());
        let backend = MacOSBackend::new().expect("backend");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &["--interrupt"], backend);
        let spin = d
            .set_breakpoint(BreakpointLocation::Line {
                file: file.clone(),
                line: fixture.marker_line("spin_line"),
            })
            .expect("break in spin");
        d.resume().expect("run to spin");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("spin stop"),
            Some(StopEvent::Breakpoint { .. })
        ));
        d.delete_breakpoint(spin).expect("remove the spin breakpoint");
        let handler = d
            .set_breakpoint(BreakpointLocation::Line {
                file,
                line: fixture.marker_line("in_handler"),
            })
            .expect("break in the handler");

        for forward in [true, false] {
            // Sent while stopped; the spinning thread takes it once resumed.
            signal(d.pid().0, "-USR1");
            d.resume().expect("resume into the signal");
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("signal stop"),
                Some(StopEvent::Signal { signal: SIGUSR1, .. })
            ));

            faults.fail_next_task_resume();
            let failed = d.resume_with_signal().expect_err("injected task_resume failure");
            assert!(failed.to_string().contains("task_resume"), "{failed}");
            assert_eq!(d.state(), SessionState::Stopped);

            if forward {
                d.resume_with_signal().expect("the signal is still held");
                assert!(matches!(
                    d.wait(Some(TIMEOUT)).expect("handler stop"),
                    Some(StopEvent::Breakpoint { .. })
                ));
                d.resume().expect("continue from the handler");
            } else {
                d.resume().expect("suppress the held signal");
            }
            // A duplicate or already-queued delivery would hit the handler again.
            assert_eq!(
                d.wait(Some(Duration::from_millis(500))).expect("no second handler stop"),
                None
            );
            d.interrupt().expect("interrupt");
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("interrupt stop"),
                Some(StopEvent::Interrupted { .. })
            ));
        }
        let hits = |d: &Session| {
            d.logical_breakpoints()
                .iter()
                .find(|bp| bp.id == handler)
                .expect("handler breakpoint")
                .hit_count
        };
        assert_eq!(hits(&d), 1, "the handler ran once");

        // After the recovery a new held signal is still forwarded.
        signal(d.pid().0, "-USR1");
        d.resume().expect("resume into the next signal");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("next signal stop"),
            Some(StopEvent::Signal { signal: SIGUSR1, .. })
        ));
        d.resume_with_signal().expect("a later forwarding continue is accepted");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("second handler stop"),
            Some(StopEvent::Breakpoint { .. })
        ));
        assert_eq!(hits(&d), 2, "the forwarded signal ran the handler");
        d.kill().expect("kill");
    });
}

/// Run the `signal_handler --interrupt` fixture to a held SIGUSR1 stop in `spin`, with a
/// breakpoint in the handler. Returns the session, its fault injector and that breakpoint.
fn held_sigusr1(
    t: &harness::TestContext,
) -> (
    harness::Debuggee,
    ferros_macos::exception::ReplyFaultInjector,
    ferros_core::LogicalBreakpointId,
)
{
    let fixture = fixture!("signal_handler");
    let file = format!("{}.rs", fixture.name());
    let backend = MacOSBackend::new().expect("backend");
    let faults = backend.reply_fault_injector();
    let mut d = t.launch_with_backend(&fixture, &["--interrupt"], backend);
    let spin = d
        .set_breakpoint(BreakpointLocation::Line {
            file: file.clone(),
            line: fixture.marker_line("spin_line"),
        })
        .expect("break in spin");
    d.resume().expect("run to spin");
    assert!(matches!(
        d.wait(Some(TIMEOUT)).expect("spin stop"),
        Some(StopEvent::Breakpoint { .. })
    ));
    d.delete_breakpoint(spin).expect("remove the spin breakpoint");
    let handler = d
        .set_breakpoint(BreakpointLocation::Line {
            file,
            line: fixture.marker_line("in_handler"),
        })
        .expect("break in the handler");
    // Sent while stopped; the spinning thread takes it once resumed.
    signal(d.pid().0, "-USR1");
    d.resume().expect("resume into the signal");
    assert!(matches!(
        d.wait(Some(TIMEOUT)).expect("signal stop"),
        Some(StopEvent::Signal { signal: SIGUSR1, .. })
    ));
    (d, faults, handler)
}

/// #250 W8: on a forwarding resume, signal selection runs after `task_resume`. Its
/// failure cannot roll the target back: the resume succeeds and `wait` reports the
/// error exactly once while the target runs.
#[test]
fn forwarding_resume_reports_a_signal_selection_failure_once_from_wait()
{
    harness::run(|t| {
        let (mut d, faults, _) = held_sigusr1(t);
        faults.fail_next_signal_selection();
        d.resume_with_signal().expect("the failure is reported by wait");
        assert_eq!(d.state(), SessionState::Running);
        let error = d.wait(Some(TIMEOUT)).expect_err("routed selection failure");
        assert!(error.to_string().contains("injected signal selection failure"), "{error}");
        assert_eq!(d.state(), SessionState::Running);
        // Reported once: nothing else is queued, and the target still runs.
        assert_eq!(d.wait(Some(Duration::from_millis(500))).expect("no second error"), None);
        d.interrupt().expect("interrupt");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("interrupt stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        d.kill().expect("kill");
    });
}

/// #250 W9: a Continue the exception loop never acknowledges makes a forwarding resume
/// wait out the reply timeout and return `Ok` with the target running; `wait` reports
/// the timeout once. The loop still handled the command, so the signal is delivered.
#[test]
fn forwarding_resume_reports_an_unacknowledged_continue_once_from_wait()
{
    harness::run(|t| {
        let (mut d, faults, handler) = held_sigusr1(t);
        faults.drop_next_continue_ack();
        let started = Instant::now();
        d.resume_with_signal().expect("the timeout is reported by wait");
        assert!(
            started.elapsed() >= Duration::from_secs(4),
            "resume returned before the reply timeout"
        );
        assert_eq!(d.state(), SessionState::Running);
        // The handler stop and the timeout reach `wait` in either order.
        let (mut errors, mut hits) = (0, 0);
        while errors + hits < 2 {
            match d.wait(Some(TIMEOUT)) {
                Err(error) => {
                    assert!(error.to_string().contains("did not answer"), "{error}");
                    errors += 1;
                }
                Ok(Some(StopEvent::Breakpoint { .. })) => hits += 1,
                other => panic!("expected the handler stop or the timeout, got {other:?}"),
            }
        }
        assert_eq!((errors, hits), (1, 1));
        let count = d
            .logical_breakpoints()
            .iter()
            .find(|bp| bp.id == handler)
            .expect("handler breakpoint")
            .hit_count;
        assert_eq!(count, 1, "the forwarded signal ran the handler once");
        d.resume().expect("continue from the handler");
        assert_eq!(d.wait(Some(Duration::from_millis(500))).expect("no second error"), None);
        d.kill().expect("kill");
    });
}

/// #250 W11: when `task_resume` fails after the task is already running and the
/// suspends cannot be put back, the backend must not report Stopped. The resume
/// succeeds, `wait` reports the failure once, and an interrupt stops the target.
#[test]
fn partial_task_resume_failure_leaves_the_target_running()
{
    harness::run(|t| {
        let (mut d, faults, _) = held_sigusr1(t);
        for forward in [true, false] {
            faults.fail_next_task_resume_partway();
            if forward {
                d.resume_with_signal().expect("forwarding resume");
            } else {
                d.resume().expect("plain resume");
            }
            assert_eq!(d.state(), SessionState::Running);
            let mut errors = 0;
            loop {
                match d.wait(Some(Duration::from_millis(500))) {
                    Err(error) => {
                        assert!(error.to_string().contains("after the task resumed"), "{error}");
                        errors += 1;
                    }
                    // The forwarded signal reaches the handler breakpoint.
                    Ok(Some(StopEvent::Breakpoint { .. })) => d.resume().expect("continue from the handler"),
                    Ok(None) => break,
                    Ok(other) => panic!("unexpected stop {other:?}"),
                }
            }
            assert_eq!(errors, 1, "the failure is reported once");
            d.interrupt().expect("interrupt a running target");
            assert!(matches!(
                d.wait(Some(TIMEOUT)).expect("interrupt stop"),
                Some(StopEvent::Interrupted { .. })
            ));
            if forward {
                // Hold a new signal for the plain resume.
                signal(d.pid().0, "-USR1");
                d.resume().expect("resume into the next signal");
                assert!(matches!(
                    d.wait(Some(TIMEOUT)).expect("signal stop"),
                    Some(StopEvent::Signal { signal: SIGUSR1, .. })
                ));
            }
        }
        d.kill().expect("kill");
    });
}

/// #58 L-3/L-5: an error after teardown must still clear the Session, and the
/// target's actual exception handlers must be restored by the safe injection hook.
#[test]
fn restore_failure_is_reported_after_detach_and_session_can_be_reused()
{
    harness::run(|t| {
        let backend = MacOSBackend::new().expect("backend");
        let faults = backend.reply_fault_injector();
        let mut d = t.launch_with_backend(&fixture!("loop_counter"), &["100000"], backend);
        faults.fail_next_exception_port_restore();
        let error = d.detach().expect_err("restore failure");
        assert!(
            matches!(error, ferros_core::error::DebuggerError::Backend(BackendError::DetachCleanup(ref message))
            if message.contains("restore exception ports"))
        );
        assert_eq!(d.state(), SessionState::NotStarted);
        assert_eq!(d.session().pid(), None);
        assert_eq!(d.ownership(), None);
        assert!(d.breakpoints().is_empty());
        // The guard retains the detached child's reaping claim independently of a
        // later process being launched in the same Session.
        d.launch(&LaunchSpec::new(fixture!("hello").path())).expect("reuse session");
        d.resume().expect("resume new child");
        assert_eq!(d.wait(Some(TIMEOUT)).expect("new exit"), Some(StopEvent::Exited { code: 0 }));
    });
}

/// Detach continues a target that was already job-control stopped before attach: it
/// runs again, exits cleanly when asked to, and is not killed.
#[test]
fn detach_resumes_a_target_stopped_before_attach()
{
    harness::run(|t| {
        let exit_gate = ReleaseFile::exit_gate(t);
        let mut target = t.spawn(&fixture!("loop_counter"), &exit_gate.gate_args());
        let pid = target.pid().0;
        signal(pid, "-STOP");
        let deadline = Instant::now() + TIMEOUT;
        while !process_state(pid).contains('T') {
            assert!(Instant::now() < deadline, "target never stopped");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut session = ferros_macos::new_session().expect("backend");
        session.attach(target.pid()).expect("attach stopped target");
        session.detach().expect("detach resumes even a previously stopped target");
        // Only a running process reads the gate opened after detach and exits 0.
        exit_gate.open_gate();
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = target.try_wait().expect("check target status") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "detach left the target unable to exit: state {}",
                process_state(pid)
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "the detached target exited with {status}");
    });
}

#[test]
fn detach_sigcont_is_visible_to_a_signal_handler()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("signal_handler"));
        d.resume().expect("run handler setup");
        assert!(d.wait_for_stdout("ready", TIMEOUT));
        d.interrupt().expect("interrupt");
        assert!(matches!(
            d.wait(Some(TIMEOUT)).expect("stop"),
            Some(StopEvent::Interrupted { .. })
        ));
        d.detach().expect("detach");
        assert!(d.wait_for_stdout("continued", TIMEOUT), "SIGCONT handler was not invoked");
    });
}

/// #58 B-7: the harness's shared handle must never wait again after the backend
/// reaped its child, while detached children are reaped by the guard that owns them.
#[test]
fn harness_reaping_claim_is_consumed_by_the_actual_reaper()
{
    const CASES: &[&str] = &["exit", "kill", "drop", "detach", "failed detach"];
    harness::run(|t| {
        for &case in CASES {
            let backend = MacOSBackend::new().expect("backend");
            let cleanup = backend.child_cleanup();
            let faults = backend.reply_fault_injector();
            let mut d = t.launch_with_backend(&fixture!("loop_counter"), &["1"], backend);
            let pid = d.pid().0;
            assert!(!cleanup.reap().expect("backend still owns the child"));
            match case {
                "exit" => {
                    d.resume().expect("resume");
                    assert_eq!(d.wait(Some(TIMEOUT)).expect("exit"), Some(StopEvent::Exited { code: 0 }));
                    assert!(!cleanup.reap().expect("watcher already reaped"));
                }
                "kill" => {
                    d.kill().expect("kill");
                    assert!(!cleanup.reap().expect("kill already reaped"));
                }
                "detach" => d.detach().expect("detach"),
                "failed detach" => {
                    faults.fail_next_exception_port_restore();
                    d.detach().expect_err("restore error");
                }
                "drop" => {}
                _ => unreachable!(),
            }
            drop(d);
            assert_reaped(pid);
            assert!(!cleanup.reap().expect("guard consumed detached claim"), "{case}");
        }
    });
}

/// Log lines written by a thread-local subscriber, for checking a backend's `tracing` output.
#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedLog
{
    /// Capture events emitted on the current thread until the guard is dropped.
    fn install(&self) -> impl Sized
    {
        use tracing_subscriber::util::SubscriberInitExt as _;
        let log = self.clone();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || log.clone())
            .finish()
            .set_default()
    }

    fn text(&self) -> String
    {
        String::from_utf8_lossy(&self.0.lock().expect("log lock")).into_owned()
    }
}

impl std::io::Write for CapturedLog
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>
    {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()>
    {
        Ok(())
    }
}

/// Attach to a running `loop_counter`, plant a trap it does not reach, resume it, and
/// make the next interrupt deliver no stop. Returns the session and the trap address.
fn attach_with_an_unreached_trap(backend: MacOSBackend, pid: ProcessId) -> (Session, ferros_core::Address)
{
    let fixture = fixture!("loop_counter");
    let drop_interrupt = backend.interrupt_fault_injector();
    let mut session = ferros_macos::session_with(backend);
    session.attach(pid).expect("attach");
    let id = session
        .set_breakpoint(BreakpointLocation::Line {
            file: format!("{}.rs", fixture.name()),
            line: fixture.marker_line("loop_final"),
        })
        .expect("set breakpoint");
    let site = session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .and_then(|breakpoint| breakpoint.sites.first())
        .expect("resolved breakpoint site")
        .address;
    session.resume().expect("resume");
    drop_interrupt();
    (session, site)
}

fn task_suspend_count(pid: ProcessId) -> i32
{
    diagnostics::process_suspend_counts(pid)
        .expect("target should still be alive")
        .task
        .expect("task suspend count")
}

/// A drop whose interrupt never stops the target holds it with a fresh task suspension:
/// the `suspends == 0` branch of `MacOSBackend::hold_attached_on_drop` (#198).
#[test]
fn attached_session_drop_suspends_the_target_when_no_stop_arrives()
{
    harness::run(|t| {
        // The loop outlives the test, so the trap after it is never reached.
        let mut target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let (session, site) = attach_with_an_unreached_trap(MacOSBackend::new().expect("backend"), pid);
        let ports = diagnostics::exception_ports(pid).expect("debugger exception ports");
        assert_eq!(task_suspend_count(pid), 0, "running target is suspended before drop");

        let log = CapturedLog::default();
        let guard = log.install();
        drop(session);
        drop(guard);
        let log = log.text();
        assert!(
            log.contains("keeping attached target stopped after waiting for a stop timed out"),
            "drop did not time out waiting for the stop:\n{log}"
        );
        assert!(!log.contains("could not suspend"), "hold failed to suspend:\n{log}");

        assert_eq!(
            target.try_wait().expect("check target status"),
            None,
            "target exited after drop"
        );
        assert!(task_suspend_count(pid) > 0, "drop left the target running");
        let state = process_state(pid.0);
        assert!(state.contains('X'), "target is no longer traced: {state}");
        assert_eq!(diagnostics::exception_ports(pid).expect("ports after drop"), ports);
        assert_eq!(
            diagnostics::raw_memory(pid, site, Arm64::TRAP_INSTRUCTION.len()).expect("read retained instruction"),
            Arm64::TRAP_INSTRUCTION
        );
        drop(target); // Harness kills and reaps the retained fixture.
        assert_reaped(pid.0);
    });
}

/// When the hold cannot suspend the target either, the failure is logged and the target
/// stays traced, with the debugger's exception ports, rather than being detached with
/// its trap still planted (#198).
#[test]
fn attached_session_drop_logs_a_hold_that_cannot_suspend()
{
    harness::run(|t| {
        let mut target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let backend = MacOSBackend::new().expect("backend");
        let fail_suspend = backend.hold_suspend_fault_injector();
        let (session, site) = attach_with_an_unreached_trap(backend, pid);
        let ports = diagnostics::exception_ports(pid).expect("debugger exception ports");

        fail_suspend();
        let log = CapturedLog::default();
        let guard = log.install();
        drop(session);
        drop(guard);
        let log = log.text();
        assert!(
            log.contains(&format!("could not suspend retained target {}", pid.0)),
            "failed hold suspension was not logged:\n{log}"
        );

        assert_eq!(
            target.try_wait().expect("check target status"),
            None,
            "target exited after drop"
        );
        assert_eq!(task_suspend_count(pid), 0, "no suspension was injected away");
        let state = process_state(pid.0);
        assert!(state.contains('X'), "target is no longer traced: {state}");
        assert_eq!(diagnostics::exception_ports(pid).expect("ports after drop"), ports);
        assert_eq!(
            diagnostics::raw_memory(pid, site, Arm64::TRAP_INSTRUCTION.len()).expect("read retained instruction"),
            Arm64::TRAP_INSTRUCTION
        );
        drop(target); // Harness kills and reaps the retained fixture.
        assert_reaped(pid.0);
    });
}

/// Set in the re-executed test binary that holds a target and then exits.
const HOLDER_TARGET_ENV: &str = "FERROS_TEST_HOLDER_TARGET";

/// What happens to a held target when the debugger process that holds it exits (#198).
///
/// The test binary re-executes itself (it carries the debugger entitlement) as a
/// separate debugger: it attaches, holds the target on drop, and exits with
/// `std::process::exit`, as the CLI does after dropping its session.
#[test]
fn held_target_when_the_debugger_process_exits()
{
    if let Ok(pid) = std::env::var(HOLDER_TARGET_ENV) {
        hold_and_exit(ProcessId(pid.parse().expect("holder target pid")));
    }
    harness::run(|t| {
        let mut target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let tag = t.tag();
        let holder = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "--nocapture", "--"])
            // libtest names it by module path inside the `native` binary.
            .arg("lifecycle::held_target_when_the_debugger_process_exits")
            .arg(tag.as_str())
            .env(HOLDER_TARGET_ENV, pid.0.to_string())
            .output()
            .expect("run holder");
        let stdout = String::from_utf8_lossy(&holder.stdout);
        let stderr = String::from_utf8_lossy(&holder.stderr);
        assert!(
            holder.status.success() && stdout.contains("holder: held"),
            "holder failed: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            holder.status
        );
        let held = stdout
            .lines()
            .find_map(|line| line.strip_prefix("holder: held "))
            .expect("holder report");
        assert!(held.contains('X'), "holder did not leave the target traced: {held}");
        assert!(
            !held.ends_with("task_suspends=0"),
            "holder did not suspend the target: {held}"
        );

        // The holder has exited; XNU's tracer-exit path must now kill its tracee.
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = target.try_wait().expect("check target status") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "held target {} outlived its debugger: state {:?}, task suspends {:?}",
                pid.0,
                process_state(pid.0),
                diagnostics::process_suspend_counts(pid).map(|counts| counts.task)
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(SIGKILL), "held target ended with {status:?}");
        drop(target);
        assert_reaped(pid.0);
    });
}

/// The holder side of [`held_target_when_the_debugger_process_exits`]: never returns.
fn hold_and_exit(pid: ProcessId) -> !
{
    let (session, _) = attach_with_an_unreached_trap(MacOSBackend::new().expect("backend"), pid);
    drop(session);
    let state = process_state(pid.0);
    let suspends = task_suspend_count(pid);
    println!("holder: held state={state} task_suspends={suspends}");
    std::process::exit(0);
}
