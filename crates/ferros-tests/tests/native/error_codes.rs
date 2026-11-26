//! Mach failures reach the `Backend` caller as a typed category that keeps the real
//! `kern_return_t` (issues #226 and #340).
//!
//! The macOS helpers used to report a failed `thread_get_state`, `vm_write`,
//! `mach_vm_protect` or `mach_vm_region_recurse` as `DebuggerError` text, which the `Backend` boundary turned into
//! `BackendError::Os { code: -1 }`: the category and the code were both lost.
#![cfg(target_os = "macos")]

use std::time::Duration;

#[cfg(target_arch = "aarch64")]
use ferros_core::types::VectorRegisterValue;
use ferros_core::{Address, BackendError, DebuggerError, MemoryRegion, StopEvent};
#[cfg(target_arch = "aarch64")]
use ferros_macos::registers::arm64::fail_next_arm64_neon_write;
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::{fixture, harness};

/// `MACH_SEND_INVALID_DEST`: Mach IPC error for a null or dead destination port.
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

#[test]
fn memory_regions_on_a_dead_task_preserves_the_mach_error_code()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("loop_counter"));
        let pid = d.pid().0;
        assert_ne!(d.memory_regions().expect("live task's memory regions"), []);

        assert!(harness::kill_tagged(d.tag()).contains(&pid), "kill fixture {pid}");
        assert!(
            harness::wait_until_gone(pid, Duration::from_secs(5)),
            "fixture {pid} was not reaped"
        );

        // Query before wait consumes the terminal event and releases the dead task port.
        let error = d.memory_regions().expect_err("enumeration on a dead task port");
        match error {
            DebuggerError::Backend(BackendError::Os { code, message }) => {
                assert_eq!(code, MACH_SEND_INVALID_DEST, "{message}");
                assert!(message.contains("mach_vm_region_recurse"), "{message}");
            }
            other => panic!("expected Os with the mach_vm_region_recurse code, got {other:?}"),
        }
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

/// `KERN_INVALID_ARGUMENT`: what a thread call returns for a port that names no thread.
#[cfg(target_arch = "aarch64")]
const KERN_INVALID_ARGUMENT: i64 = 4;

/// A debug-register call whose `thread_get_state` fails with `KERN_INVALID_ARGUMENT`
/// keeps that code (issue #341). It used to go through `From<MachError>`, which made it
/// `DebuggerError::InvalidArgument`, so `BackendError::Unsupported` at the `Backend`
/// boundary, with no code.
///
/// The port is this process's own task port: the kernel answers a thread call on a task
/// with `KERN_INVALID_ARGUMENT` every time. (A thread that has exited answers it only
/// briefly before its port becomes a dead name, `MACH_SEND_INVALID_DEST`.) The calls go
/// to the kernel, not to a debuggee, so this test launches no fixture.
#[cfg(target_arch = "aarch64")]
#[test]
fn debug_register_calls_on_a_port_that_is_not_a_thread_keep_kern_invalid_argument()
{
    use ferros_macos::{breakpoints, registers};

    harness::run(|_| {
        let port = registers::debug::not_a_thread_port();
        let failures = vec![
            ("set_single_step", registers::set_single_step(port, true).err()),
            (
                "clear_hardware_breakpoint",
                registers::clear_hardware_breakpoint(port, 0).err(),
            ),
            ("clear_watchpoint", registers::clear_watchpoint(port, 0).err()),
            (
                "install_hardware_breakpoint",
                breakpoints::install_hardware_breakpoint(&[port], Address::from(0x1000u64)).err(),
            ),
            (
                "install_watchpoint",
                breakpoints::install_watchpoint(
                    &[port],
                    Address::from(0x1000u64),
                    8,
                    ferros_core::WatchpointAccess::ReadWrite,
                )
                .err(),
            ),
        ];
        for (call, error) in failures {
            match error {
                Some(DebuggerError::Backend(BackendError::Os { code, ref message })) => {
                    assert_eq!(code, KERN_INVALID_ARGUMENT, "{call}: {message}");
                    assert!(message.contains("thread_get_state"), "{call}: {message}");
                }
                other => panic!("{call} on a task port: expected Os {{ code: 4 }}, got {other:?}"),
            }
        }
    });
}

/// `KERN_NO_SPACE`: what macOS 15 returns for a refused `VM_PROT_COPY` on a permanent page.
const KERN_NO_SPACE: i64 = 3;

/// A write to a permanent page whose maximum protection is read-only is refused by the
/// kernel's `mach_vm_protect(VM_PROT_COPY)`, and the page's bytes do not change.
///
/// The refusal differs by release (checked against apple-oss-distributions/xnu):
/// - Since xnu-12377.101 (macOS 26.4 and later, including the macOS 27 this was
///   written on), `vm_map_delete` refuses to overwrite a permanent entry before
///   touching it, so `mach_vm_protect` fails with `KERN_PROTECTION_FAILURE` (2) and the
///   page keeps its `r--` protection.
/// - On xnu-11417 (macOS 15, the `macos-15` CI runner) and xnu-12377 before .101,
///   the overwrite leaves the permanent entry in place but strips it to `---/---`, then
///   finds the entry still there and fails with `KERN_NO_SPACE` (3). The page cannot
///   be read through that mapping any more.
///
/// So the fixture maps the bytes twice, and the backing page, an ordinary mapping of
/// the same memory, shows them whichever way the kernel refused.
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
        let (view, backing) = stdout
            .lines()
            .find_map(|line| line.strip_prefix("read-only page: 0x")?.split_once(" backing: 0x"))
            .expect("page address line");
        let parse = |hex: &str| Address::from(u64::from_str_radix(hex, 16).expect("page address"));
        let (address, backing) = (parse(view), parse(backing));
        let pid = d.pid();
        let before = diagnostics::raw_memory(pid, address, SPAN).expect("read the original bytes");
        assert_eq!(before, [0xa5; SPAN]);
        assert_eq!(
            diagnostics::raw_memory(pid, backing, SPAN).expect("read the backing page"),
            before,
            "the view and the backing page share memory"
        );

        let error = d
            .session_mut()
            .backend_mut()
            .write_memory(address, &[0x5a; SPAN])
            .expect_err("write to the permanent read-only page");
        // The refusal comes from mach_vm_protect itself, with the release's code. The rest
        // of the message is not checked: on macOS 15 the engine also reports a failed
        // rollback, because the stripped page can no longer be read back.
        let view_after = diagnostics::raw_memory(pid, address, SPAN);
        match error {
            BackendError::PermissionDenied(ref message)
                if message.starts_with("mach_vm_protect ") && message.ends_with("(kern_return_t 2)") =>
            {
                let view_after = view_after.expect("the view stays readable after a KERN_PROTECTION_FAILURE");
                assert_eq!(view_after, before, "failed write changed the view");
            }
            BackendError::Os {
                code: KERN_NO_SPACE,
                ref message,
            } if message.starts_with("mach_vm_protect ") => {
                // The kernel strips a refused permanent view to no access on these
                // releases; if it is readable, it must hold its original bytes.
                if let Ok(view_after) = view_after {
                    assert_eq!(view_after, before, "failed write changed the view");
                }
            }
            ref other => panic!(
                "write at {address}: expected mach_vm_protect's KERN_PROTECTION_FAILURE or KERN_NO_SPACE, got {other:?}"
            ),
        }
        let after = diagnostics::raw_memory(pid, backing, SPAN).expect("reread the backing page without the cache");
        assert_eq!(after, before, "failed write changed target memory");
    });
}
