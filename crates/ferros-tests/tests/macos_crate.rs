//! The macOS debugger as its own crate: `ferros_macos::MacOSDebugger` is reachable from
//! the platform crate, not from `ferros-core`, and reports portable `DebuggerError`s.
#![cfg(target_os = "macos")]

use std::process::Command;

use ferros_core::{Debugger, DebuggerError, ProcessId};
use ferros_macos::MacOSDebugger;
use ferros_tests::{fixture, harness};

/// Attaching to a pid whose process has exited reports the portable `ProcessNotFound`.
///
/// No fixture is launched (the pid comes from `/usr/bin/true`, which has already been
/// reaped), so nothing can leak and the test does not need `harness::run`.
#[test]
fn attach_to_exited_pid_returns_process_not_found()
{
    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn /usr/bin/true");
    let pid = child.id();
    let status = child.wait().expect("wait for /usr/bin/true");
    assert!(status.success(), "/usr/bin/true failed: {status}");

    let mut debugger = MacOSDebugger::new().expect("MacOSDebugger::new");
    match debugger.attach(ProcessId::from(pid)) {
        Err(DebuggerError::ProcessNotFound(reported)) => assert_eq!(reported, pid),
        other => panic!("expected ProcessNotFound({pid}), got {other:?}"),
    }
}

/// The harness launches fixtures through `ferros_macos::MacOSDebugger`.
#[test]
fn launch_through_macos_crate_reports_host_architecture()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        assert!(d.is_stopped(), "process should be stopped at entry");
        #[cfg(target_arch = "aarch64")]
        assert!(
            matches!(d.architecture(), ferros_core::Architecture::Arm64),
            "unexpected architecture {:?}",
            d.architecture()
        );
    });
}
