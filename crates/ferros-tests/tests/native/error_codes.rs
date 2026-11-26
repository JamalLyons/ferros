//! Mach failures reach the `Backend` caller as a typed category that keeps the real
//! `kern_return_t` (issue #226).
//!
//! The macOS helpers used to report a failed `thread_get_state`, `vm_write` or
//! `mach_vm_protect` as `DebuggerError` text, which the `Backend` boundary turned into
//! `BackendError::Os { code: -1 }`: the category and the code were both lost.
#![cfg(target_os = "macos")]

#[cfg(target_arch = "aarch64")]
use ferros_core::types::VectorRegisterValue;
use ferros_core::{Address, BackendError, DebuggerError, MemoryRegion};
use ferros_macos::MacOSBackend;
#[cfg(target_arch = "aarch64")]
use ferros_macos::registers::arm64::fail_next_arm64_neon_write;
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

#[cfg(target_arch = "aarch64")]
#[test]
fn failed_neon_write_restores_general_registers()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let thread = d.selected_thread().expect("selected thread");
        let backend = d.session_mut().backend_mut();
        let before = backend.read_registers(thread).expect("original register state");
        assert!(!before.vector.is_empty(), "fixture must expose NEON state");
        let mut updated = before.clone();
        updated.general[0] ^= 1;
        updated.vector[0] = VectorRegisterValue::from_u128(before.vector[0].as_u128() ^ 1);

        fail_next_arm64_neon_write(false);
        let error = backend.write_registers(thread, &updated).expect_err("NEON write must fail");
        assert!(
            matches!(error, BackendError::Os { code: MACH_SEND_INVALID_DEST, ref message }
                if message.contains("thread_set_state(ARM_NEON_STATE64)")),
            "unexpected NEON write error: {error:?}"
        );

        let after = backend.read_registers(thread).expect("register state after failed write");
        assert_eq!(after.general[0], before.general[0], "failed NEON write changed x0");
        assert_eq!(after.general, before.general);
        assert_eq!(
            (after.pc, after.sp, after.fp, after.status),
            (before.pc, before.sp, before.fp, before.status)
        );
        assert_eq!(after.vector, before.vector);
        assert_eq!(after.floating, before.floating);

        backend.write_registers(thread, &updated).expect("retry after one-shot fault");
        let after_retry = backend.read_registers(thread).expect("register state after retry");
        assert_eq!(after_retry.general[0], updated.general[0]);
        assert_eq!(after_retry.vector[0], updated.vector[0]);
    });
}

#[cfg(target_arch = "aarch64")]
#[test]
fn failed_gpr_rollback_keeps_neon_error_category_and_reports_both_failures()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let thread = d.selected_thread().expect("selected thread");
        let backend = d.session_mut().backend_mut();
        let mut updated = backend.read_registers(thread).expect("original register state");
        assert!(!updated.vector.is_empty(), "fixture must expose NEON state");
        updated.general[0] ^= 1;

        fail_next_arm64_neon_write(true);
        let error = backend.write_registers(thread, &updated).expect_err("NEON write must fail");
        let BackendError::Os { code, message } = error else {
            panic!("unexpected NEON write error: {error:?}");
        };
        assert_eq!(code, MACH_SEND_INVALID_DEST, "{message}");
        assert!(message.starts_with("thread_set_state(ARM_NEON_STATE64)"), "{message}");
        assert!(
            message.contains("also failed") && message.contains("thread_set_state(ARM_THREAD_STATE64)"),
            "{message}"
        );

        let after = backend.read_registers(thread).expect("register state after failed rollback");
        assert_eq!(after.general[0], updated.general[0]);
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
