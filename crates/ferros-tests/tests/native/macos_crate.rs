//! The macOS backend as its own crate: `ferros_macos::MacOSBackend` is reachable from
//! the platform crate, not from `ferros-core`, and reports portable errors through
//! `ferros_core::Session`.
#![cfg(target_os = "macos")]

use std::process::Command;

use ferros_core::backend::Backend;
use ferros_core::{Architecture, BackendError, DebuggerError, LaunchSpec, ProcessId, SessionState};
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness};
use object::macho::CPU_TYPE_X86_64;

/// Attaching to a pid whose process has exited reports the portable `NotFound`.
///
/// No fixture is launched (the pid comes from `/usr/bin/true`, which has already been
/// reaped), so nothing can leak and the test does not need `harness::run`.
#[test]
fn attach_to_exited_pid_returns_not_found()
{
    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn /usr/bin/true");
    let pid = child.id();
    let status = child.wait().expect("wait for /usr/bin/true");
    assert!(status.success(), "/usr/bin/true failed: {status}");

    let mut session = ferros_macos::new_session().expect("MacOSBackend::new");
    match session.attach(ProcessId::from(pid)) {
        Err(DebuggerError::Backend(BackendError::NotFound(_))) => {}
        other => panic!("expected Backend(NotFound) for pid {pid}, got {other:?}"),
    }
    assert_eq!(session.state(), SessionState::NotStarted);
}

#[test]
fn attach_permission_denial_explains_signing_and_launch()
{
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let mut backend = MacOSBackend::new().expect("backend");
        backend.fail_next_task_for_pid();
        let err = backend.attach(target.pid()).expect_err("task-port request should fail");
        let BackendError::PermissionDenied(detail) = err else {
            panic!("expected PermissionDenied, got {err:?}");
        };
        assert!(detail.contains("com.apple.security.cs.debugger"), "{detail}");
        assert!(detail.contains("just sign-debugger"), "{detail}");
        assert!(detail.contains("ferros launch <program>"), "{detail}");
        assert!(!detail.contains("sudo ferros"), "{detail}");
    });
}

/// The harness launches fixtures through `ferros_macos::MacOSBackend`.
#[test]
fn launch_through_macos_crate_reports_target_architecture()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        assert_eq!(d.state(), SessionState::Stopped, "process should be stopped at entry");
        assert_eq!(d.architecture(), Architecture::Arm64);
        assert_eq!(
            d.backend().arch().expect("target architecture").architecture(),
            Architecture::Arm64
        );
    });
}

#[test]
fn launch_rejects_an_unsupported_target_cpu_type()
{
    harness::run(|t| {
        let fixture = fixture!("hello");
        let mut backend = MacOSBackend::new().expect("backend");
        // Exercise rejection on hosts where the kernel cannot launch a translated executable.
        backend.override_next_target_cpu_type(CPU_TYPE_X86_64.0);
        let tag = t.tag();
        let result = backend.launch(&LaunchSpec::new(fixture.path()).arg(tag.as_str()));
        assert!(
            matches!(&result, Err(BackendError::Unsupported(message)) if message.contains("x86-64")),
            "expected unsupported x86-64 target, got {result:?}"
        );
        assert!(
            harness::tagged_pids(tag.as_str()).is_empty(),
            "rejected child survived launch"
        );

        let retry = t.tag();
        backend
            .launch(&LaunchSpec::new(fixture.path()).arg(retry.as_str()))
            .expect("retry launch");
        assert_eq!(
            backend.arch().expect("target architecture").architecture(),
            Architecture::Arm64
        );
        backend.kill().expect("kill launched target");
        assert_eq!(backend.architecture(), Architecture::current());
    });
}

#[test]
fn attach_rejects_an_unsupported_target_cpu_type()
{
    harness::run(|t| {
        let target = t.spawn(&fixture!("loop_counter"), &["100000"]);
        let pid = target.pid();
        let mut backend = MacOSBackend::new().expect("backend");
        backend.override_next_target_cpu_type(CPU_TYPE_X86_64.0);
        let result = backend.attach(pid);
        assert!(
            matches!(&result, Err(BackendError::Unsupported(message)) if message.contains("x86-64")),
            "expected unsupported x86-64 target, got {result:?}"
        );

        backend.attach(pid).expect("retry attach to the same target");
        assert_eq!(
            backend.arch().expect("target architecture").architecture(),
            Architecture::Arm64
        );
        backend.detach().expect("detach supported target");
        assert_eq!(backend.architecture(), Architecture::current());
    });
}
