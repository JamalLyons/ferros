//! Process lifecycle through `ferros_core::Session` on the macOS backend.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferros_core::backend::{Backend, BackendError, ResumeRequest};
use ferros_core::{LaunchSpec, ProcessId, ProcessOwnership, Session, SessionState, StopEvent};
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

/// A nonfatal replacement is passed once, without exposing a second signal stop.
#[test]
fn held_signal_can_be_replaced_with_a_nonfatal_signal()
{
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &["25"]);
        let pid = d.pid().0;
        signal(pid, "-USR1");
        d.resume().expect("resume to signal");
        let thread = match d.wait(Some(TIMEOUT)).expect("signal stop") {
            Some(StopEvent::Signal { signal: SIGUSR1, thread }) => thread,
            other => panic!("expected SIGUSR1, got {other:?}"),
        };
        // Darwin SIGCONT is 19. Its default action is nonfatal.
        d.session_mut()
            .backend_mut()
            .resume(&ResumeRequest::continue_all().with_signal(thread, 19))
            .expect("replace SIGUSR1 with SIGCONT");
        assert_eq!(
            d.wait(Some(TIMEOUT)).expect("exit without duplicate stop"),
            Some(StopEvent::Exited { code: 0 })
        );
        assert!(d.wait_for_stdout("final count: 25", TIMEOUT));
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
        let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
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

/// A fixture started through `/bin/sh`, so its parent is the shell and not this test
/// process: attaching reparents it to the debugger and detaching hands it back, as for a
/// process started from a terminal. The shell waits for it, so the shell exits when the
/// fixture does.
struct Grandchild
{
    shell: std::process::Child,
    pid: u32,
}

impl Grandchild
{
    fn spawn(args: &[&str]) -> Self
    {
        let shell = Command::new("/bin/sh")
            .arg("-c")
            // Not just `"$@"`: the shell would exec the fixture instead of waiting for it.
            .arg("\"$@\"; status=$?; exit $status")
            .arg("sh")
            .arg(fixture!("loop_counter").path())
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        let deadline = Instant::now() + TIMEOUT;
        let pid = loop {
            let output = Command::new("/usr/bin/pgrep")
                .args(["-P", &shell.id().to_string()])
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
        Self { shell, pid }
    }

    /// Whether the fixture (and so the shell) exited by itself within `timeout`, with
    /// status 0.
    fn ran_to_completion(&mut self, timeout: Duration) -> Option<std::process::ExitStatus>
    {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.shell.try_wait() {
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
        let _ = Command::new("/bin/kill").args(["-9", &self.pid.to_string()]).status();
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}

/// A process the debugger releases runs on to completion: it is not left stopped by the
/// private SIGSTOP that ends tracing, and a user signal held when it is released is
/// discarded, not delivered.
///
/// The released process must not be in state `T`, and `loop_counter 100` must run its
/// second or so to completion and exit 0 (a delivered SIGUSR1 would kill it). The
/// target's parent is a shell, as for a process started from a terminal (see
/// [`Grandchild`]).
#[test]
fn released_processes_run_to_completion()
{
    const SHORT_LOOP: &str = "100";
    const EXIT_TIMEOUT: Duration = Duration::from_secs(15);
    /// How the attached process is released.
    const CASES: &[&str] = &[
        "detach after inspecting",
        "detach",
        "drop stopped",
        "drop running",
        "backend drop",
        "detach with held signal",
    ];

    harness::run_with_timeout(Duration::from_secs(120), |t| {
        for &case in CASES {
            let mut target = Grandchild::spawn(&[SHORT_LOOP]);
            let pid = ProcessId::from(target.pid);
            let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
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
            // Not left job-control stopped. (`ps` state is reliable here: the target
            // was not launched suspended.) The shell would not show it: it continues a
            // stopped child itself.
            for _ in 0..5 {
                let stat = process_state(target.pid);
                assert!(
                    !stat.starts_with('T'),
                    "{case}: the released process is stopped (state {stat})"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            let status = target
                .ran_to_completion(EXIT_TIMEOUT)
                .unwrap_or_else(|| panic!("{case}: the released process did not run to completion"));
            assert!(status.success(), "{case}: the released process exited with {status}");
        }

        // A launched process released with detach also runs to completion.
        let mut d = t.launch_with_args(&fixture!("loop_counter"), &[SHORT_LOOP]);
        d.detach().expect("detach a launched process");
        assert!(
            d.wait_for_stdout("final count: 100", EXIT_TIMEOUT),
            "the detached launched process did not run to completion"
        );
    });
}

/// Other signals must never let user code run while the signal stop is held (#56).
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

#[test]
fn detach_resumes_a_target_stopped_before_attach()
{
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        signal(target.pid().0, "-STOP");
        let deadline = Instant::now() + TIMEOUT;
        while !process_state(target.pid().0).contains('T') {
            assert!(Instant::now() < deadline, "target never stopped");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
        session.attach(target.pid()).expect("attach stopped target");
        session.detach().expect("detach resumes even a previously stopped target");
        let deadline = Instant::now() + TIMEOUT;
        while process_state(target.pid().0).contains('T') {
            assert!(Instant::now() < deadline, "detach left target stopped");
            std::thread::sleep(Duration::from_millis(10));
        }
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
