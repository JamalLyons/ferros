//! Lifecycle tests: `detach`.

use std::fmt::Write as _;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use ferros_core::backend::{Backend, BackendError};
use ferros_core::{BreakpointLocation, LaunchSpec, ProcessId, Session, SessionState, StopEvent};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness};

use super::{SIGUSR1, TIMEOUT, assert_reaped, process_state, signal};

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
