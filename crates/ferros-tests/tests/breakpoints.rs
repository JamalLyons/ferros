//! Software breakpoints through `ferros_core::Session` on the macOS backend: the engine
//! plants trap bytes with `Backend::write_memory` and maps the raw pc of a trap stop.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{BreakpointRequest, SessionState, StopEvent, arch};
use ferros_tests::{fixture, harness};

/// How long a test waits for the breakpoint stop after resuming.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// arm64 `brk #0`, little endian.
#[cfg(target_arch = "aarch64")]
const ARM64_TRAP: [u8; 4] = [0x00, 0x00, 0x20, 0xD4];

#[test]
fn breakpoint_writes_trap_and_remove_restores_original_bytes()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let pc = d.read_registers().expect("read_registers").pc;
        let original = d.read_memory(pc, 4).expect("read original bytes");

        let id = d
            .add_breakpoint(BreakpointRequest::Software { address: pc })
            .expect("add_breakpoint at the entry pc");
        let with_trap = d.read_memory(pc, 4).expect("read bytes with the trap");
        let trap = arch::for_architecture(d.architecture())
            .expect("architecture with trap support")
            .trap_instruction();
        assert_eq!(&with_trap[..trap.len()], trap, "trap bytes not written at {pc}");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(with_trap, ARM64_TRAP, "arm64 brk #0 not written at {pc}");
        assert_ne!(with_trap, original, "the entry instruction already was a trap");

        d.remove_breakpoint(id).expect("remove_breakpoint");
        let restored = d.read_memory(pc, 4).expect("read restored bytes");
        assert_eq!(restored, original, "original bytes not restored at {pc}");
        assert!(
            d.breakpoints().is_empty(),
            "breakpoint table not empty: {:?}",
            d.breakpoints()
        );
    });
}

#[test]
fn resume_stops_at_breakpoint_on_entry_pc()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let entry_thread = d.selected_thread().expect("a selected thread at entry");
        let pc = d.read_registers().expect("read_registers").pc;
        let id = d
            .add_breakpoint(BreakpointRequest::Software { address: pc })
            .expect("add_breakpoint at the entry pc");

        d.resume().expect("resume");
        let event = d.wait(Some(STOP_TIMEOUT)).expect("wait");
        match event {
            Some(StopEvent::Breakpoint { thread, pc: stop_pc }) => {
                assert_eq!(stop_pc, pc, "breakpoint stop at the wrong pc");
                assert_eq!(thread, entry_thread, "breakpoint stop on an unexpected thread");
                assert_eq!(d.selected_thread(), Some(thread), "the stopping thread is not selected");
            }
            other => panic!("expected a breakpoint stop at {pc} within {STOP_TIMEOUT:?}, got {other:?}"),
        }
        assert_eq!(d.state(), SessionState::Stopped);
        assert_eq!(d.breakpoint_info(id).expect("breakpoint_info").hit_count, 1);
        assert!(
            matches!(d.last_stop(), Some(StopEvent::Breakpoint { .. })),
            "last_stop: {:?}",
            d.last_stop()
        );
        // The trap is still in place and every thread is stopped: the pc reads back as is.
        assert_eq!(d.read_registers().expect("read_registers at the stop").pc, pc);
    });
}

// Known gap: continuing from a breakpoint stop does not step over the trap yet, so the
// thread hits the same trap again and the fixture never prints. Issue #33 (T07) adds
// stepping over breakpoints and makes this test pass.
#[test]
#[ignore = "T07: Session::resume does not step over the trap yet (issue #33)"]
fn continue_past_breakpoint_runs_to_completion()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let pc = d.read_registers().expect("read_registers").pc;
        d.add_breakpoint(BreakpointRequest::Software { address: pc })
            .expect("add_breakpoint at the entry pc");

        d.resume().expect("resume");
        let event = d.wait(Some(STOP_TIMEOUT)).expect("wait");
        assert!(
            matches!(event, Some(StopEvent::Breakpoint { .. })),
            "expected a breakpoint stop, got {event:?}"
        );

        d.resume().expect("resume past the breakpoint");
        assert!(
            d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)),
            "fixture output never arrived after continuing; stdout: {:?}, stderr: {:?}",
            d.stdout(),
            d.stderr()
        );
    });
}
