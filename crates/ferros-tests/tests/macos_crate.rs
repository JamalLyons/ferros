//! The macOS backend as its own crate: `ferros_macos::MacOSBackend` is reachable from
//! the platform crate, not from `ferros-core`, and reports portable errors through
//! `ferros_core::Session`.
#![cfg(target_os = "macos")]

use std::process::Command;

use ferros_core::{BackendError, DebuggerError, ProcessId, Session, SessionState};
use ferros_macos::MacOSBackend;
use ferros_tests::{fixture, harness};

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

    let mut session = Session::new(Box::new(MacOSBackend::new().expect("MacOSBackend::new")));
    match session.attach(ProcessId::from(pid)) {
        Err(DebuggerError::Backend(BackendError::NotFound(_))) => {}
        other => panic!("expected Backend(NotFound) for pid {pid}, got {other:?}"),
    }
    assert_eq!(session.state(), SessionState::NotStarted);
}

/// The harness launches fixtures through `ferros_macos::MacOSBackend`.
#[test]
fn launch_through_macos_crate_reports_host_architecture()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        assert_eq!(d.state(), SessionState::Stopped, "process should be stopped at entry");
        #[cfg(target_arch = "aarch64")]
        assert!(
            matches!(d.architecture(), ferros_core::Architecture::Arm64),
            "unexpected architecture {:?}",
            d.architecture()
        );
    });
}
