//! Mach failures reach the `Backend` caller as a typed category that keeps the real
//! `kern_return_t` (issue #226).
//!
//! The macOS helpers used to report a failed `thread_get_state`, `vm_write` or
//! `mach_vm_protect` as `DebuggerError` text, which the `Backend` boundary turned into
//! `BackendError::Os { code: -1 }`: the category and the code were both lost.
#![cfg(target_os = "macos")]

use ferros_core::{Address, BackendError, DebuggerError, MemoryRegion};
use ferros_macos::MacOSBackend;
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

/// `KERN_INVALID_ARGUMENT`: what a thread call returns for a port that names no thread.
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
