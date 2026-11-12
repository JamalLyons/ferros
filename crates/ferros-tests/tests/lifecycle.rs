//! Process lifecycle through `ferros_core::Session` on the macOS backend.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{SessionState, StopEvent};
use ferros_tests::{fixture, harness};

// Known gap: the macOS backend does not watch for process exit yet, so `wait` never
// reports `Exited`. Issue #32 (T06) adds exit detection and makes this test pass. The
// finite timeout makes the test fail instead of hanging when ignored tests are run.
#[test]
#[ignore = "T06: the macOS backend does not report process exit yet (issue #32)"]
fn wait_reports_exit_code()
{
    harness::run(|t| {
        // The exit_code fixture takes no arguments and exits with status 7.
        let mut d = t.launch(&fixture!("exit_code"));
        d.resume().expect("resume");
        let event = d.wait(Some(Duration::from_secs(10))).expect("wait");
        assert_eq!(event, Some(StopEvent::Exited { code: 7 }));
        assert_eq!(d.state(), SessionState::Exited);
    });
}
