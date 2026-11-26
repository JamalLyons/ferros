//! Mach failures reach the `Backend` caller as a typed category that keeps the real
//! `kern_return_t` (issue #226).
//!
//! The macOS helpers used to report a failed `thread_get_state`, `vm_write` or
//! `mach_vm_protect` as `DebuggerError` text, which the `Backend` boundary turned into
//! `BackendError::Os { code: -1 }`: the category and the code were both lost.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{Address, BackendError, DebuggerError, MemoryRegion, StopEvent};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness};

/// `MACH_SEND_INVALID_DEST`: what a Mach call on `MACH_PORT_NULL` returns.
const MACH_SEND_INVALID_DEST: i64 = 0x1000_0003;

/// Bytes written across the end of a region: half before it, half after.
const SPAN: usize = 16;

#[test]
fn failed_thread_get_state_reaches_read_registers_with_its_code()
{
    harness::run(|t| {
        let backend = MacOSBackend::new().expect("backend");
        let fail_read = backend.register_read_fault_injector();
        let mut d = t.launch_with_backend(&fixture!("loop_counter"), &[], backend);
        d.session_mut().read_registers().expect("registers before the fault");

        fail_read();
        let error = d.session_mut().read_registers().expect_err("read of MACH_PORT_NULL");
        match error {
            DebuggerError::Backend(BackendError::Os { code, ref message }) => {
                assert_eq!(code, MACH_SEND_INVALID_DEST, "{message}");
                assert!(message.contains("thread_get_state"), "{message}");
            }
            other => panic!("expected Os with the thread_get_state code, got {other:?}"),
        }
        d.session_mut().read_registers().expect("registers after the one-shot fault");
    });
}

/// The target's memory regions, sorted by start address.
fn regions(d: &mut harness::Debuggee) -> Vec<MemoryRegion>
{
    let mut regions = d.session_mut().backend_mut().memory_regions().expect("memory regions");
    regions.sort_by_key(|region| region.start);
    regions
}

/// A write that runs from one mapped region into the next is refused before any byte
/// changes. It used to reach the caller as `Os { code: -1 }` from `ensure_range`'s
/// I/O text; it is now `Unsupported`, which says what was refused.
#[test]
fn write_across_a_region_boundary_is_unsupported()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let regions = regions(&mut d);
        let boundary = regions
            .windows(2)
            .find(|pair| {
                pair[0].permissions.starts_with('r') && pair[1].permissions.starts_with('r') && pair[0].end == pair[1].start
            })
            .map(|pair| pair[1].start)
            .expect("two adjacent readable regions");
        let at = Address::from(boundary.value() - SPAN as u64 / 2);
        let before = d.session_mut().backend_mut().read_memory(at, SPAN).expect("span is readable");

        let error = d
            .session_mut()
            .backend_mut()
            .write_memory(at, &[0; SPAN])
            .expect_err("write across the boundary");
        assert!(
            matches!(error, BackendError::Unsupported(ref message) if message.contains("region boundary")),
            "write at {at}: {error:?}"
        );
        assert_eq!(d.session_mut().backend_mut().read_memory(at, SPAN).expect("reread"), before);
    });
}

#[test]
fn write_that_runs_into_an_unmapped_hole_is_not_found()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let regions = regions(&mut d);
        let end = regions
            .windows(2)
            .find(|pair| pair[0].permissions.starts_with("rw") && pair[1].start.value() >= pair[0].end.value() + SPAN as u64)
            .map(|pair| pair[0].end)
            .expect("a writable region followed by an unmapped hole");
        let at = Address::from(end.value() - SPAN as u64 / 2);

        let error = d
            .session_mut()
            .backend_mut()
            .write_memory(at, &[0; SPAN])
            .expect_err("write into the hole");
        assert!(matches!(error, BackendError::NotFound(_)), "write at {at}: {error:?}");
    });
}

#[test]
fn refused_memory_protection_change_is_permission_denied_and_preserves_bytes()
{
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("soft_faults"), &["--read-only-page"]);
        let timeout = Duration::from_secs(5);
        d.resume().expect("run until the read-only page is ready");
        assert!(
            d.wait_for_stdout("\n", timeout),
            "fixture did not publish the page: {}",
            d.stderr()
        );
        d.interrupt().expect("interrupt the fixture");
        let stop = d.wait(Some(timeout)).expect("wait for the interrupt");
        assert!(matches!(stop, Some(StopEvent::Interrupted { .. })), "got {stop:?}");

        let stdout = d.stdout();
        let hex = stdout
            .lines()
            .find_map(|line| line.strip_prefix("read-only page: 0x"))
            .expect("page address line");
        let address = Address::from(u64::from_str_radix(hex, 16).expect("page address"));
        let pid = d.pid();
        let before = diagnostics::raw_memory(pid, address, SPAN).expect("read the original bytes");
        assert_eq!(before, [0xa5; SPAN]);

        let error = d
            .session_mut()
            .backend_mut()
            .write_memory(address, &[0x5a; SPAN])
            .expect_err("write to the permanent read-only page");
        // Require the kernel's KERN_PROTECTION_FAILURE from mach_vm_protect itself.
        assert!(
            matches!(error, BackendError::PermissionDenied(ref message)
                if message.starts_with("mach_vm_protect ") && message.ends_with("(kern_return_t 2)")),
            "write at {address}: {error:?}"
        );
        let after = diagnostics::raw_memory(pid, address, SPAN).expect("reread the target bytes without the cache");
        assert_eq!(after, before, "failed write changed target memory");
    });
}
