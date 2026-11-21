//! The portable `Backend` trait and its in-memory `MockBackend`, used from outside
//! `ferros-core` through the public API only (the `test-util` feature).
//!
//! Unlike every other file here, these tests launch no real process: the mock is a
//! scripted, in-memory backend. They therefore need no `harness::run`, no fixtures and no
//! debugger entitlement, and they are not limited to macOS.

use ferros_core::backend::mock::{MockBackend, MockCall, MockCallKind, MockState, MockStop, arm64_registers};
use ferros_core::backend::{Backend, BackendError, BackendResult, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::types::{Address, ProcessId, ThreadId};
use ferros_core::{BreakpointLocation, LogicalBreakpointId, Session, SessionState, arch};

const THREAD: ThreadId = ThreadId(1);
const BREAKPOINT: Address = Address::new(0x1_0000_0008);

fn scripted_process() -> MockBackend
{
    let mut registers = arm64_registers();
    registers.pc = Address::new(0x1_0000_0000);
    let mock = MockBackend::new()
        .with_pid(ProcessId(4242))
        .with_thread(THREAD, registers)
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_pc(THREAD, BREAKPOINT)
        .expect_resume(ResumeRequest::continue_all()),
    );
    mock.script(MockStop::new(StopEvent::SingleStep { thread: THREAD }).expect_resume(ResumeRequest::step(THREAD)));
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    mock
}

/// Continue past one breakpoint the way an engine would, through any `Backend`.
fn continue_past_breakpoint(backend: &mut dyn Backend) -> BackendResult<Vec<StopEvent>>
{
    let arch = arch::for_architecture(backend.architecture())?;
    let trap = arch.trap_instruction();
    let mut events = Vec::new();

    backend.launch(&LaunchSpec::new("/bin/prog"))?;
    let original = backend.read_memory(BREAKPOINT, trap.len())?;
    backend.write_memory(BREAKPOINT, trap)?;
    backend.resume(&ResumeRequest::continue_all())?;
    events.extend(backend.wait(None)?);
    let registers = backend.read_registers(THREAD)?;
    assert_eq!(arch.pc(&registers), BREAKPOINT);
    backend.write_memory(BREAKPOINT, &original)?;
    backend.resume(&ResumeRequest::step(THREAD))?;
    events.extend(backend.wait(None)?);
    backend.write_memory(BREAKPOINT, trap)?;
    backend.resume(&ResumeRequest::continue_all())?;
    events.extend(backend.wait(None)?);
    Ok(events)
}

#[test]
fn boxed_mock_runs_on_another_thread()
{
    let mock = scripted_process();
    let mut backend: Box<dyn Backend> = Box::new(mock.clone());
    let events = std::thread::spawn(move || continue_past_breakpoint(backend.as_mut()))
        .join()
        .expect("engine thread panicked")
        .expect("backend call failed");

    assert_eq!(
        events,
        vec![
            StopEvent::Breakpoint {
                thread: THREAD,
                pc: BREAKPOINT
            },
            StopEvent::SingleStep { thread: THREAD },
            StopEvent::Exited { code: 0 },
        ]
    );
    assert_eq!(mock.state(), MockState::NoProcess);
    assert_eq!(mock.script_len(), 0);
}

#[test]
fn breakpoint_step_exit_records_exact_calls()
{
    let mut mock = scripted_process();
    continue_past_breakpoint(&mut mock).expect("backend call failed");

    let trap = arch::Arm64::TRAP_INSTRUCTION.to_vec();
    assert_eq!(
        mock.take_calls(),
        vec![
            MockCall::Launch(LaunchSpec::new("/bin/prog")),
            MockCall::ReadMemory {
                address: BREAKPOINT,
                len: 4
            },
            MockCall::WriteMemory {
                address: BREAKPOINT,
                data: trap.clone()
            },
            MockCall::Resume(ResumeRequest::continue_all()),
            MockCall::Wait(None),
            MockCall::ReadRegisters(THREAD),
            MockCall::WriteMemory {
                address: BREAKPOINT,
                data: vec![0x1F; 4]
            },
            MockCall::Resume(ResumeRequest::step(THREAD)),
            MockCall::Wait(None),
            MockCall::WriteMemory {
                address: BREAKPOINT,
                data: trap.clone()
            },
            MockCall::Resume(ResumeRequest::continue_all()),
            MockCall::Wait(None),
        ]
    );
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(trap));
}

#[test]
fn detach_cleanup_error_clears_session_but_ordinary_error_keeps_it_stopped()
{
    use ferros_core::backend::BackendError;
    use ferros_core::backend::mock::MockCallKind;
    use ferros_core::{BreakpointRequest, Session, SessionState};

    for teardown in [false, true] {
        let mock = scripted_process();
        let mut session = Session::new(Box::new(mock.clone()));
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        session
            .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
            .expect("breakpoint");
        let error = if teardown {
            BackendError::DetachCleanup("restore failed".into())
        } else {
            BackendError::Os {
                code: 1,
                message: "detach refused".into(),
            }
        };
        mock.fail_next(MockCallKind::Detach, error.clone());
        let returned = session.detach().expect_err("injected detach error");
        assert!(matches!(returned, ferros_core::error::DebuggerError::Backend(ref actual) if *actual == error));
        if teardown {
            assert_eq!(session.state(), SessionState::NotStarted);
            assert_eq!(mock.state(), MockState::NoProcess);
            assert_eq!(session.pid(), None);
            assert_eq!(session.ownership(), None);
            assert_eq!(session.selected_thread(), None);
            assert!(session.breakpoints().is_empty());
            session.launch(&LaunchSpec::new("/bin/prog")).expect("reuse session");
        } else {
            assert_eq!(session.state(), SessionState::Stopped);
            assert_eq!(mock.state(), MockState::Stopped);
            assert!(session.breakpoints().iter().all(|bp| !bp.enabled));
            session.detach().expect("retry detach");
        }
    }
}

#[test]
fn kill_cleanup_error_clears_session_but_ordinary_error_preserves_it()
{
    use ferros_core::backend::BackendError;
    use ferros_core::backend::mock::MockCallKind;
    use ferros_core::{BreakpointRequest, Session, SessionState};

    for teardown in [false, true] {
        let mock = scripted_process();
        let mut session = Session::new(Box::new(mock.clone()));
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        session
            .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
            .expect("breakpoint");
        let error = if teardown {
            BackendError::KillCleanup("reaping failed".into())
        } else {
            BackendError::PermissionDenied("kill refused".into())
        };
        mock.fail_next(MockCallKind::Kill, error);
        session.kill().expect_err("injected kill failure");
        if teardown {
            assert_eq!(session.state(), SessionState::Exited);
            assert_eq!(mock.state(), MockState::NoProcess);
            assert_eq!(session.pid(), None);
            assert_eq!(session.ownership(), None);
            assert_eq!(session.selected_thread(), None);
            assert_eq!(session.last_stop(), None);
            assert!(session.breakpoints().is_empty());
            session.launch(&LaunchSpec::new("/bin/prog")).expect("reuse session");
        } else {
            assert_eq!(session.state(), SessionState::Stopped);
            assert_eq!(mock.state(), MockState::Stopped);
            assert!(session.breakpoints().iter().all(|bp| bp.enabled));
            session.kill().expect("retry kill");
        }
    }
}

#[test]
fn public_nth_failure_is_one_shot_and_counts_matching_calls()
{
    let mut mock = scripted_process();
    mock.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let error = BackendError::PermissionDenied("injected".into());
    mock.fail_on_nth(MockCallKind::ReadMemory, 2, error.clone());
    assert!(mock.read_memory(BREAKPOINT, 4).is_ok());
    assert!(mock.threads().is_ok());
    assert_eq!(mock.read_memory(BREAKPOINT, 4), Err(error));
    assert!(mock.read_memory(BREAKPOINT, 4).is_ok());
}

#[test]
fn session_carries_process_group_to_backend()
{
    use ferros_core::Session;
    use ferros_core::backend::ProcessGroup;

    for group in [None, Some(ProcessGroup::New)] {
        let mock = MockBackend::new();
        let mut session = Session::new(Box::new(mock.clone()));
        let mut spec = LaunchSpec::new("/bin/prog");
        assert_eq!(spec.process_group, None, "default must inherit");
        if let Some(group) = group {
            spec = spec.process_group(group);
        }
        session.launch(&spec).expect("launch");
        assert_eq!(spec.process_group, group);
        assert_eq!(mock.take_calls().first(), Some(&MockCall::Launch(spec)));
    }
}

/// A portable, synthetic arm64 Mach-O with two DWARF subprograms named `multi`.
/// No compiler, fixture process, or platform API is needed to resolve multiple sites.
struct MultiSiteImage(std::path::PathBuf);

impl MultiSiteImage
{
    fn new() -> (Self, MockBackend)
    {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferros-mock-multi-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // DWARF 4: a compile unit with children, then subprograms with inline name,
        // low_pc (address), high_pc (data4 offset). No statement table is referenced.
        let abbrev = vec![1, 0x11, 1, 0, 0, 2, 0x2e, 0, 3, 8, 0x11, 1, 0x12, 6, 0, 0, 0];
        let mut info = vec![0; 4];
        info.extend_from_slice(&[4, 0, 0, 0, 0, 0, 8, 1]);
        for address in [BREAKPOINT, SECOND_SITE] {
            info.extend_from_slice(b"\x02multi\0");
            info.extend_from_slice(&address.value().to_le_bytes());
            info.extend_from_slice(&4_u32.to_le_bytes());
        }
        info.push(0);
        let length = u32::try_from(info.len() - 4).unwrap();
        info[..4].copy_from_slice(&length.to_le_bytes());

        let sections =
            vec![("__debug_info", info), ("__debug_abbrev", abbrev), ("__debug_line", vec![0])].into_boxed_slice();
        let commands_size = 72 + 72 + 80 * sections.len();
        let mut bytes = vec![0; 32 + commands_size];
        let word = |bytes: &mut [u8], offset, value: u32| bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        let long = |bytes: &mut [u8], offset, value: u64| bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        word(&mut bytes, 0, 0xfeed_facf);
        word(&mut bytes, 4, 0x0100_000c);
        word(&mut bytes, 12, 2);
        word(&mut bytes, 16, 2);
        word(&mut bytes, 20, u32::try_from(commands_size).unwrap());
        word(&mut bytes, 32, 0x19); // LC_SEGMENT_64: header and code mapping.
        word(&mut bytes, 36, 72);
        bytes[40..46].copy_from_slice(b"__TEXT");
        long(&mut bytes, 56, IMAGE_BASE.value());
        long(&mut bytes, 64, 0x2000);
        long(&mut bytes, 80, 0x2000);
        word(&mut bytes, 88, 5);
        word(&mut bytes, 92, 5);
        let dwarf = 104;
        word(&mut bytes, dwarf, 0x19);
        word(&mut bytes, dwarf + 4, u32::try_from(72 + 80 * sections.len()).unwrap());
        bytes[dwarf + 8..dwarf + 15].copy_from_slice(b"__DWARF");
        word(&mut bytes, dwarf + 64, u32::try_from(sections.len()).unwrap());
        for (index, (name, data)) in sections.into_iter().enumerate() {
            let section = dwarf + 72 + 80 * index;
            bytes[section..section + name.len()].copy_from_slice(name.as_bytes());
            bytes[section + 16..section + 23].copy_from_slice(b"__DWARF");
            long(&mut bytes, section + 40, data.len() as u64);
            let offset = u32::try_from(bytes.len()).unwrap();
            word(&mut bytes, section + 48, offset);
            bytes.extend(data);
        }
        bytes.resize(0x2000, 0x1F);
        std::fs::write(&path, &bytes).expect("write synthetic image");
        let mut registers = arm64_registers();
        registers.pc = IMAGE_BASE;
        let mock = MockBackend::new()
            .with_thread(THREAD, registers)
            .with_region(IMAGE_BASE, "r-x", bytes)
            .with_image(ferros_core::backend::LoadedImage {
                path: path.clone(),
                load_address: IMAGE_BASE,
            });
        (Self(path), mock)
    }
}

impl Drop for MultiSiteImage
{
    fn drop(&mut self)
    {
        let _ = std::fs::remove_file(&self.0);
    }
}

const IMAGE_BASE: Address = Address::new(0xFFFF_F000);
const SECOND_SITE: Address = Address::new(0x1_0000_0010);

fn multi_site_session() -> (MultiSiteImage, MockBackend, Session, LogicalBreakpointId)
{
    let (image, mock) = MultiSiteImage::new();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let id = session
        .set_breakpoint(BreakpointLocation::Function("multi".into()))
        .expect("multi-site breakpoint");
    assert_eq!(
        session.logical_breakpoints()[0]
            .sites
            .iter()
            .map(|site| site.address)
            .collect::<Vec<_>>(),
        vec![BREAKPOINT, SECOND_SITE]
    );
    (image, mock, session, id)
}

fn write_error() -> BackendError
{
    BackendError::Os {
        code: 5,
        message: "injected trap write failure".into(),
    }
}

fn assert_failed_logical_release_rolls_back(delete: bool)
{
    // Issue #107: both removal and disabling, including the shared-disabled-entry path.
    for shared in [false, true] {
        let (_image, mock, mut session, id) = multi_site_session();
        if shared {
            let other = session.set_breakpoint(BreakpointLocation::Function("multi".into())).unwrap();
            session.disable_logical_breakpoint(other).unwrap();
        }
        let before = session.breakpoints();
        let logical_before = session.logical_breakpoints().to_vec();
        mock.fail_on_nth(MockCallKind::WriteMemory, 2, write_error());
        let result = if delete {
            session.delete_breakpoint(id)
        } else {
            session.disable_logical_breakpoint(id)
        };
        assert!(result.is_err());
        assert_eq!(session.logical_breakpoints(), logical_before);
        let after = session.breakpoints();
        assert_eq!(after.len(), before.len());
        for (actual, expected) in after.iter().zip(before) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.address, expected.address);
            assert_eq!(actual.enabled, expected.enabled);
            assert_eq!(actual.state, expected.state);
            assert_eq!(actual.requested_at, expected.requested_at);
            assert_eq!(actual.resolved_at, expected.resolved_at);
            assert_eq!(actual.hit_count, expected.hit_count);
            assert_eq!(
                mock.peek_memory(actual.address, 4),
                Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec())
            );
        }
    }
}

#[test]
fn failed_logical_delete_restores_every_site_and_table_entry()
{
    assert_failed_logical_release_rolls_back(true);
}

#[test]
fn failed_logical_disable_restores_every_site_and_table_entry()
{
    assert_failed_logical_release_rolls_back(false);
}

#[test]
fn successful_detach_preserves_source_breakpoint_intent()
{
    let (_image, mock, mut session, id) = multi_site_session();
    let disabled = session.set_breakpoint(BreakpointLocation::Function("multi".into())).unwrap();
    session.disable_logical_breakpoint(disabled).unwrap();
    session.detach().unwrap();
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.logical_breakpoints()[0].id, id);
    assert!(session.logical_breakpoints()[0].enabled);
    assert!(!session.logical_breakpoints()[1].enabled);
    assert!(
        session
            .logical_breakpoints()
            .iter()
            .all(|bp| bp.sites.is_empty() && bp.pending.is_some())
    );
    session.launch(&LaunchSpec::new("/bin/prog")).unwrap();
    assert_eq!(session.logical_breakpoints()[0].sites.len(), 2);
    assert!(session.breakpoints().iter().all(|bp| bp.enabled));
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

#[test]
fn retried_detach_after_a_backend_failure_keeps_source_breakpoint_intent()
{
    use ferros_core::backend::BackendError;

    let (_image, mock, mut session, id) = multi_site_session();
    let explicit = session.set_breakpoint(BreakpointLocation::Function("multi".into())).unwrap();
    mock.fail_next(
        MockCallKind::Detach,
        BackendError::Os {
            code: 1,
            message: "detach refused".into(),
        },
    );
    session.detach().expect_err("injected detach error");
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(
        session.logical_breakpoints().iter().all(|bp| !bp.enabled),
        "every trap is out"
    );
    // Disabled explicitly between the attempts: stays disabled.
    session.disable_logical_breakpoint(explicit).unwrap();
    session.detach().expect("retry detach");
    let enabled: Vec<_> = session.logical_breakpoints().iter().map(|bp| (bp.id, bp.enabled)).collect();
    assert_eq!(enabled, [(id, true), (explicit, false)]);
    session.launch(&LaunchSpec::new("/bin/prog")).unwrap();
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

#[test]
fn failed_detach_restore_disables_every_owner_of_restored_sites()
{
    let (_image, mock, mut session, _) = multi_site_session();
    session.set_breakpoint(BreakpointLocation::Address(BREAKPOINT)).unwrap();
    let unaffected = session.set_breakpoint(BreakpointLocation::Address(SECOND_SITE)).unwrap();
    mock.fail_on_nth(MockCallKind::WriteMemory, 2, write_error());
    session.detach().expect_err("second restore fails");
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.state(), MockState::Stopped);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));
    assert_eq!(mock.peek_memory(SECOND_SITE, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    for breakpoint in session.logical_breakpoints() {
        assert_eq!(breakpoint.enabled, breakpoint.id == unaffected);
    }
    assert!(!session.breakpoints()[0].enabled);
    assert!(session.breakpoints()[1].enabled);
}

#[test]
fn failed_logical_release_rollback_reports_unarmed_owners_disabled()
{
    for delete in [false, true] {
        let (_image, mock, mut session, id) = multi_site_session();
        mock.fail_on_nth(MockCallKind::WriteMemory, 2, write_error());
        mock.fail_next(
            MockCallKind::WriteMemory,
            BackendError::PermissionDenied("rollback refused".into()),
        );
        let result = if delete {
            session.delete_breakpoint(id)
        } else {
            session.disable_logical_breakpoint(id)
        };
        assert!(matches!(result, Err(ferros_core::DebuggerError::Backend(error)) if error == write_error()));
        assert!(!session.logical_breakpoints()[0].enabled);
        assert_eq!(session.breakpoints().len(), 2);
        assert!(!session.breakpoints()[0].enabled);
        assert!(session.breakpoints()[1].enabled);
        assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));
        assert_eq!(mock.peek_memory(SECOND_SITE, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
        session.delete_breakpoint(id).expect("delete remaining armed site");
        assert_eq!(session.logical_breakpoints(), []);
        assert!(session.breakpoints().is_empty());
        assert_eq!(mock.peek_memory(SECOND_SITE, 4), Ok(vec![0x1F; 4]));
    }
}

#[test]
fn failed_resume_rearm_disables_every_logical_owner()
{
    // Issue #134: the re-arm after a failed `backend.resume` in `start_plan`.
    let (_image, mock, mut session, _) = multi_site_session();
    session.set_breakpoint(BreakpointLocation::Address(BREAKPOINT)).unwrap();
    let unaffected = session.set_breakpoint(BreakpointLocation::Address(SECOND_SITE)).unwrap();
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_pc(THREAD, BREAKPOINT),
    );
    session.resume().unwrap();
    session.wait(None).unwrap();
    mock.fail_next(MockCallKind::Resume, BackendError::PermissionDenied("resume refused".into()));
    // First write restores the original instruction; the second is the re-arm.
    mock.fail_on_nth(MockCallKind::WriteMemory, 2, write_error());
    let result = session.step_instruction(THREAD);
    assert!(matches!(result, Err(ferros_core::DebuggerError::Backend(error)) if error == write_error()));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));
    assert!(!session.breakpoints()[0].enabled);
    for breakpoint in session.logical_breakpoints() {
        assert_eq!(breakpoint.enabled, breakpoint.id == unaffected);
    }
}

#[test]
fn failed_step_rearm_disables_every_logical_owner()
{
    let (_image, mock, mut session, _) = multi_site_session();
    session.set_breakpoint(BreakpointLocation::Address(BREAKPOINT)).unwrap();
    let unaffected = session.set_breakpoint(BreakpointLocation::Address(SECOND_SITE)).unwrap();
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_pc(THREAD, BREAKPOINT),
    );
    session.resume().unwrap();
    session.wait(None).unwrap();
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    session.step_instruction(THREAD).unwrap();
    mock.fail_next(MockCallKind::WriteMemory, write_error());
    session.wait(None).expect_err("rearm fails");
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));
    assert!(!session.breakpoints()[0].enabled);
    for breakpoint in session.logical_breakpoints() {
        assert_eq!(breakpoint.enabled, breakpoint.id == unaffected);
    }
    // Re-enabling a partially armed owner recovers all of its sites.
    let id = session.logical_breakpoints()[0].id;
    session.enable_logical_breakpoint(id).unwrap();
    assert!(session.logical_breakpoints()[0].enabled);
    assert!(session.breakpoints().iter().all(|bp| bp.enabled));
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

const STACK: Address = Address::new(0x7_0000_0000);
const CALLEE: Address = Address::new(0x1_0000_0000);

/// A process stopped in a frame whose return address is `BREAKPOINT`: frame pointer and
/// stack pointer at `STACK`, which holds saved fp 0 and saved lr `BREAKPOINT`.
fn returning_to_breakpoint() -> MockBackend
{
    let mut stack = vec![0u8; 32];
    stack[8..16].copy_from_slice(&BREAKPOINT.value().to_le_bytes());
    MockBackend::new()
        .with_thread(THREAD, in_callee(arm64_registers()))
        .with_region(CALLEE, "r-x", vec![0x1F; 64])
        .with_region(STACK, "rw-", stack)
}

fn in_callee(mut registers: ferros_core::Registers) -> ferros_core::Registers
{
    registers.pc = CALLEE;
    registers.fp = STACK;
    registers.sp = STACK;
    registers
}

/// `step_out` from the callee, with the next stop scripted as the return trap.
fn step_out_to_breakpoint(mock: &MockBackend, session: &mut Session)
{
    let caller = session.stack_trace(8).unwrap()[1].clone();
    assert_eq!(caller.pc, BREAKPOINT);
    let mut at_return = session.read_registers_for(THREAD).unwrap();
    at_return.pc = BREAKPOINT;
    at_return.sp = caller.sp;
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_registers(THREAD, at_return),
    );
    session.step_out().unwrap();
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

#[test]
fn enabling_a_breakpoint_at_a_line_step_site_keeps_it_enabled_and_armed()
{
    // Issue #133: a line step puts a temporary trap on a disabled site and disables the
    // site again when the step ends. The session is running for the whole step, so
    // enabling a logical breakpoint there is refused and the release cannot undo it.
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let id = session.set_breakpoint(BreakpointLocation::Address(BREAKPOINT)).unwrap();
    session.disable_logical_breakpoint(id).unwrap();
    step_out_to_breakpoint(&mock, &mut session);
    assert!(matches!(
        session.enable_logical_breakpoint(id),
        Err(ferros_core::DebuggerError::NotStopped)
    ));
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: THREAD }));
    assert!(!session.logical_breakpoints()[0].enabled);
    assert_eq!(session.logical_breakpoints()[0].hit_count, 0);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));

    // Enabled once the step has stopped, it is armed; the next line step through the
    // site reports the hit and leaves it enabled and armed.
    session.enable_logical_breakpoint(id).unwrap();
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    let back = in_callee(session.read_registers_for(THREAD).unwrap());
    session.write_registers_for(THREAD, &back).unwrap();
    step_out_to_breakpoint(&mock, &mut session);
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT
        })
    );
    assert!(session.logical_breakpoints()[0].enabled);
    assert_eq!(session.logical_breakpoints()[0].hit_count, 1);
    assert!(session.breakpoints()[0].enabled);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
}

/// Complement the real signal-handler step-resume test with the exact ordering: a user hit, a signal
/// interrupting the instruction step before it executes, then handler return to
/// that same pc. This ordering cannot be forced by sending a signal to a stopped
/// process: the kernel may deliver it after the instruction completes instead. The real
/// handler test is `tests/stepping.rs::signal_handler_return_does_not_count_the_step_resume_breakpoint`.
#[test]
fn signal_during_breakpoint_step_does_not_report_or_count_a_second_hit()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    let thread = ThreadId(1);
    let address = Address::new(0x1000);
    let next = address + 4;
    let mut registers = arm64_registers();
    registers.pc = address + 16;
    registers.sp = Address::new(0x8000);
    let mock = MockBackend::new()
        .with_thread(thread, registers)
        .with_region(address, "r-x", vec![0; 64]);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("scripted-signal-step")).expect("launch mock");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address })
        .expect("breakpoint");
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: address })
            .set_pc(thread, address)
            .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("run to user hit");
    assert!(matches!(
        session.wait(None).expect("wait").expect("stop"),
        StopEvent::Breakpoint { .. }
    ));
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    mock.script(MockStop::new(StopEvent::Signal { thread, signal: 30 }).expect_resume(ResumeRequest::step(thread)));
    session.resume().expect("signal interrupts original instruction");
    assert!(matches!(
        session.wait(None).expect("wait").expect("stop"),
        StopEvent::Signal { .. }
    ));
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: address })
            .set_pc(thread, address)
            .expect_resume(
                ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(thread, ThreadAction::Continue)
                    .with_signal(thread, 30),
            ),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread })
            .set_pc(thread, next)
            .expect_resume(ResumeRequest::step(thread)),
    );
    mock.script(MockStop::new(StopEvent::Interrupted { thread: Some(thread) }).expect_resume(ResumeRequest::continue_all()));
    session
        .resume_with_signal()
        .expect("handler returns, original instruction executes");
    assert_eq!(
        session.wait(None).expect("wait").expect("stop"),
        StopEvent::Interrupted { thread: Some(thread) }
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    assert_eq!(mock.peek_registers(thread).expect("registers").pc, next);
}

/// A signal passed at breakpoint A whose handler stops at breakpoint B leaves A's
/// step-resume rendezvous pending. Removing and re-adding A drops it, so the thread's
/// next arrival at A in the same frame is a real hit (issue #120 review).
#[test]
fn removing_the_breakpoint_drops_a_stopped_handlers_rendezvous()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    let thread = ThreadId(1);
    let original = Address::new(0x1000);
    let handler = Address::new(0x1020);
    let mut registers = arm64_registers();
    registers.pc = original;
    registers.sp = Address::new(0x8000);
    let mock = MockBackend::new()
        .with_thread(thread, registers.clone())
        .with_region(original, "r-x", vec![0; 64]);
    let mut session = Session::new(Box::new(mock.clone()));
    session
        .launch(&LaunchSpec::new("scripted-handler-stop"))
        .expect("launch mock");
    mock.script(MockStop::new(StopEvent::Signal { thread, signal: 30 }).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Signal { .. })));
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: original })
        .expect("breakpoint at the signal pc");
    let in_handler = session
        .add_breakpoint(BreakpointRequest::Software { address: handler })
        .expect("breakpoint in the handler");
    let mut handler_registers = arm64_registers();
    handler_registers.pc = handler;
    handler_registers.sp = Address::new(0x7000);
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: handler })
            .set_registers(thread, handler_registers)
            .expect_resume(
                ResumeRequest::all(ThreadAction::Suspend)
                    .with_thread(thread, ThreadAction::Continue)
                    .with_signal(thread, 30),
            ),
    );
    session.resume_with_signal().expect("pass the signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Breakpoint { pc, .. }) if pc == handler));

    session.remove_breakpoint(id).expect("remove the original breakpoint");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: original })
        .expect("re-add it");
    session.remove_breakpoint(in_handler).expect("remove the handler breakpoint");
    // The handler returns to the original pc, in the frame it interrupted.
    mock.script(
        MockStop::new(StopEvent::Breakpoint { thread, pc: original })
            .set_registers(thread, registers)
            .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("let the handler return");
    assert_eq!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { thread, pc: original })
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
}

/// `step_out` from the callee, raced by an interrupt: the interrupt is reported first and
/// the return trap's exception stays queued behind it, as on a real kernel when the
/// thread reaches the trap just as it is stopped. The step ends and removes its trap,
/// so the queued stop is left over from a trap that is gone (issue #142). `more` are
/// further stops queued after it.
fn step_out_interrupted_at_return_trap(mock: &MockBackend, session: &mut Session, more: Vec<MockStop>)
{
    let caller = session.stack_trace(8).unwrap()[1].clone();
    assert_eq!(caller.pc, BREAKPOINT);
    let mut at_return = session.read_registers_for(THREAD).unwrap();
    at_return.pc = BREAKPOINT;
    at_return.sp = caller.sp;
    let mut stops = vec![
        MockStop::new(StopEvent::Interrupted { thread: Some(THREAD) }),
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
        .set_registers(THREAD, at_return),
    ];
    stops.extend(more);
    mock.script_simultaneous(stops);
    session.step_out().unwrap();
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Interrupted { thread: Some(THREAD) })
    );
    // The step is over: its private trap is gone, but its exception is still queued.
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]));
    assert!(session.breakpoints().iter().all(|bp| bp.address != BREAKPOINT));
    assert!(mock.pending_len() > 0);
}

#[test]
fn resume_drops_a_stale_return_trap_stop_and_continues()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    step_out_interrupted_at_return_trap(&mock, &mut session, Vec::new());

    // Resume drains the stale stop, then really continues the process.
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("resume");
    assert_eq!(mock.pending_len(), 0);
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(mock.script_len(), 0);
}

#[test]
fn step_instruction_drops_a_stale_return_trap_stop_and_steps()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    step_out_interrupted_at_return_trap(&mock, &mut session, Vec::new());

    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    session.step_instruction(THREAD).expect("step");
    assert_eq!(mock.pending_len(), 0);
    assert_eq!(session.wait(None).unwrap(), Some(StopEvent::SingleStep { thread: THREAD }));
    assert_eq!(mock.script_len(), 0);
    assert_eq!(session.read_registers_for(THREAD).unwrap().pc, BREAKPOINT + 4);
}

#[test]
fn resume_still_reports_a_queued_stop_at_an_armed_breakpoint()
{
    use ferros_core::BreakpointRequest;

    // A second thread is stopped at a real breakpoint in the same group, queued behind
    // the stale return-trap stop. Only the stale one is dropped.
    let other = ThreadId(2);
    let user = CALLEE + 16;
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session.select_thread(THREAD).expect("select the stepping thread");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: user })
        .expect("user breakpoint");
    step_out_interrupted_at_return_trap(
        &mock,
        &mut session,
        vec![MockStop::new(StopEvent::Breakpoint { thread: other, pc: user }).set_pc(other, user)],
    );
    assert_eq!(session.breakpoints().len(), 1);

    session.resume().expect("resume");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint { thread: other, pc: user })
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
    assert_eq!(mock.pending_len(), 0);
}

#[test]
fn resume_reports_a_drained_breakpoint_stop_at_an_armed_breakpoint()
{
    use ferros_core::BreakpointRequest;

    // Two threads stop at real breakpoints at once: `wait` reports the first, and the
    // second, drained by the next `resume`, still holds a trap, so it is reported too.
    let other = ThreadId(2);
    let first = CALLEE + 16;
    let second = CALLEE + 32;
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 40;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let a = session
        .add_breakpoint(BreakpointRequest::Software { address: first })
        .expect("first breakpoint");
    let b = session
        .add_breakpoint(BreakpointRequest::Software { address: second })
        .expect("second breakpoint");
    mock.script_simultaneous(vec![
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: first,
        })
        .set_pc(THREAD, first)
        .expect_resume(ResumeRequest::continue_all()),
        MockStop::new(StopEvent::Breakpoint {
            thread: other,
            pc: second,
        })
        .set_pc(other, second),
    ]);
    session.resume().expect("resume");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: THREAD,
            pc: first
        })
    );
    session.resume().expect("resume with a stop queued");
    assert_eq!(
        session.wait(None).unwrap(),
        Some(StopEvent::Breakpoint {
            thread: other,
            pc: second
        })
    );
    assert_eq!(session.breakpoint_info(a).expect("first").hit_count, 1);
    assert_eq!(session.breakpoint_info(b).expect("second").hit_count, 1);
}

/// #150: a stop can arrive after `Session::resume` drained the backend's queue and before
/// the backend checks it, so the backend refuses the resume with `StopPending`. The
/// session defers that stop as it does a drained one: it becomes running, the next
/// `wait` reports the stop, and the step over the breakpoint at the pc is undone.
#[test]
fn resume_reports_a_stop_that_arrives_after_its_drain()
{
    let mock = scripted_process();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint");
    session.resume().expect("run to the breakpoint");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { .. })
    ));

    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    let _ = mock.take_calls();
    session.resume().expect("a stop raced the resume");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.state(), MockState::Stopped, "nothing ran");
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.wait(None).expect("wait"), Some(late.clone()));
    assert_eq!(session.last_stop(), Some(&late));
    assert_eq!(session.breakpoints()[0].hit_count, 1);

    // The process runs on from there: the breakpoint is stepped over and the exit reported.
    session.resume().expect("resume after the late stop");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150: the same race for `step_instruction`: the late stop is reported, not an error.
#[test]
fn step_instruction_reports_a_stop_that_arrives_after_its_drain()
{
    let mock = scripted_process();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.step_instruction(THREAD).expect("a stop raced the step");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    assert_eq!(mock.script_len(), 3, "no scripted stop was consumed");
}

/// #150: the same race for a source step that runs to a private return trap: the late
/// stop is reported and the private trap is gone with the step.
#[test]
fn step_out_reports_a_stop_that_arrives_after_its_drain()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.step_out().expect("a stop raced the step");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(vec![0x1F; 4]), "private trap released");
    assert!(session.breakpoints().is_empty());
    assert_eq!(session.wait(None).expect("wait"), Some(late));
}

/// #150: the race can also hit the final continue of `resume`'s step over the breakpoint
/// at the pc: the late stop is reported from `wait`, the trap is armed again, and the
/// signal meant for the continue stays held for the next `resume_with_signal`.
#[test]
fn step_over_reports_a_stop_that_arrives_before_its_final_continue()
{
    let mock = scripted_process();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint");
    session.resume().expect("run to the breakpoint");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { .. })
    ));

    // Resume 1 is the step off the breakpoint (scripted SingleStep); resume 2 is the
    // final continue, which the late stop races.
    let late = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(late.clone())));
    session.resume().expect("a stop raced the final continue");
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(mock.state(), MockState::Stopped, "the final continue ran nothing");
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    assert_eq!(session.breakpoints()[0].hit_count, 1);
    assert_eq!(mock.script_len(), 1, "only the exit is left");
}

/// The `Resume` requests `mock` received since its calls were last taken.
fn resumes(mock: &MockBackend) -> Vec<ResumeRequest>
{
    mock.take_calls()
        .into_iter()
        .filter_map(|call| match call {
            MockCall::Resume(request) => Some(request),
            _ => None,
        })
        .collect()
}

/// #150 review: a `resume_with_signal` refused for a stop that arrived after its drain
/// sent nothing, so the signal it meant to pass is still held: the next
/// `resume_with_signal` passes it, together with the late stop's.
#[test]
fn signal_of_a_refused_resume_is_passed_by_the_next_resume_with_signal()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = Address::new(0x1_0000_0020);
    let mock = MockBackend::new()
        .with_thread(THREAD, registers.clone())
        .with_thread(other, registers)
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let first = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.script(MockStop::new(first.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(first));

    let late = StopEvent::Signal {
        thread: other,
        signal: 31,
    };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.resume_with_signal().expect("a stop raced the resume");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    let _ = mock.take_calls();
    let both = ResumeRequest::continue_all().with_signal(THREAD, 30).with_signal(other, 31);
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("pass both signals");
    assert_eq!(resumes(&mock), vec![both]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150 review: a source step's final continue (which passes no signal) refused for a
/// late stop keeps a signal already carried for a suspended thread, from an earlier
/// step-over that another stop cut short; the next `resume_with_signal` passes it.
#[test]
fn refused_step_out_keeps_a_signal_carried_for_another_thread()
{
    use ferros_core::BreakpointRequest;

    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = CALLEE + 32;
    let mock = returning_to_breakpoint().with_thread(other, registers);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let signal = StopEvent::Signal {
        thread: other,
        signal: 30,
    };
    mock.script(MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));

    // The step over the breakpoint at THREAD's pc is cut short, so the signal meant for
    // the final continue is carried.
    session
        .add_breakpoint(BreakpointRequest::Software { address: CALLEE })
        .expect("breakpoint at the pc");
    let cut = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.script(MockStop::new(cut.clone()).expect_resume(ResumeRequest::step(THREAD)));
    session.resume_with_signal().expect("step over the breakpoint");
    assert_eq!(session.wait(None).expect("wait"), Some(cut));

    // step_out steps THREAD over its breakpoint (resume 1); the late stop races its final
    // continue (resume 2), which suspends the other thread and passes no signal.
    session.select_thread(THREAD).expect("select");
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, CALLEE + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    let late = StopEvent::Interrupted { thread: Some(other) };
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(late.clone())));
    session.step_out().expect("step out");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    let _ = mock.take_calls();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("pass the carried signal");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all().with_signal(other, 30)]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150 review: a step over a breakpoint whose start is refused for a late stop ran
/// nothing, so a stopped handler's rendezvous at that breakpoint is kept: its later return
/// there is not reported or counted as a second hit.
#[test]
fn refused_step_over_keeps_a_stopped_handlers_rendezvous()
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    // `handler` is the thread whose signal handler stops; `stepper` later sits at the
    // same breakpoint and is stepped over it.
    let stepper = ThreadId(1);
    let handler = ThreadId(2);
    let original = Address::new(0x1000);
    let in_handler = Address::new(0x1020);
    let mut registers = arm64_registers();
    registers.pc = original;
    registers.sp = Address::new(0x8000);
    let mut elsewhere = registers.clone();
    elsewhere.pc = original + 16;
    let mock = MockBackend::new()
        .with_thread(stepper, elsewhere)
        .with_thread(handler, registers.clone())
        .with_region(original, "r-x", vec![0; 64]);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("scripted-refused-step")).expect("launch");
    mock.script(
        MockStop::new(StopEvent::Signal {
            thread: handler,
            signal: 30,
        })
        .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("run to the signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Signal { .. })));
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: original })
        .expect("breakpoint at the signal pc");
    let handler_bp = session
        .add_breakpoint(BreakpointRequest::Software { address: in_handler })
        .expect("breakpoint in the handler");
    let mut handler_registers = registers.clone();
    handler_registers.pc = in_handler;
    handler_registers.sp = Address::new(0x7000);
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: handler,
            pc: in_handler,
        })
        .set_registers(handler, handler_registers)
        .expect_resume(
            ResumeRequest::all(ThreadAction::Suspend)
                .with_thread(handler, ThreadAction::Continue)
                .with_signal(handler, 30),
        ),
    );
    session.resume_with_signal().expect("pass the signal");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { pc, .. }) if pc == in_handler
    ));
    session.remove_breakpoint(handler_bp).expect("remove the handler breakpoint");

    // The other thread arrives at the original breakpoint; the step over it is refused.
    let mut at_original = session.read_registers_for(stepper).expect("registers");
    at_original.pc = original;
    session.write_registers_for(stepper, &at_original).expect("move");
    let late = StopEvent::Interrupted { thread: Some(stepper) };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.resume().expect("a stop raced the step over");
    assert_eq!(session.wait(None).expect("wait"), Some(late));
    assert_eq!(mock.peek_memory(original, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));

    // The user moves the other thread off the breakpoint (a step over it that runs would
    // remove the trap, which gives the rendezvous up). The handler then returns to the
    // original breakpoint in the frame it interrupted: the rendezvous, so its instruction
    // is executed and the continue goes on to the interrupt.
    let mut moved = session.read_registers_for(stepper).expect("registers");
    moved.pc = original + 16;
    session.write_registers_for(stepper, &moved).expect("move off");
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: handler,
            pc: original,
        })
        .set_registers(handler, registers)
        .expect_resume(ResumeRequest::continue_all()),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: handler })
            .set_pc(handler, original + 4)
            .expect_resume(ResumeRequest::step(handler)),
    );
    let end = StopEvent::Interrupted { thread: Some(handler) };
    mock.script(MockStop::new(end.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(end));
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
    assert_eq!(mock.script_len(), 0);
}

/// #150 review: an instruction step that fails to start ran nothing, so the stepping
/// thread's carried signal is still held and the next `resume_with_signal` passes it.
#[test]
fn failed_step_instruction_keeps_the_threads_carried_signal()
{
    let mock = MockBackend::new().with_thread(THREAD, arm64_registers()).with_region(
        Address::new(0x1_0000_0000),
        "r-x",
        vec![0x1F; 64],
    );
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let signal = StopEvent::Signal {
        thread: THREAD,
        signal: 30,
    };
    mock.script(MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    // The signal is carried: a late stop refuses the resume that would pass it.
    let late = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
    session.resume_with_signal().expect("a stop raced the resume");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    mock.fail_next(
        MockCallKind::Resume,
        BackendError::Os {
            code: 5,
            message: "step failed".into(),
        },
    );
    assert!(session.step_instruction(THREAD).is_err());
    assert_eq!(session.state(), SessionState::Stopped);

    let _ = mock.take_calls();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("pass the carried signal");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all().with_signal(THREAD, 30)]);
}

/// The thread whose signal handler stops in the `handler_stopped_*` tests.
const HANDLER: ThreadId = ThreadId(2);
/// Where `HANDLER` is interrupted by its signal; a breakpoint is set there.
const INTERRUPTED: Address = Address::new(0x1000);

/// Two threads: `THREAD` at `INTERRUPTED + 16` and `HANDLER` at `INTERRUPTED`, where
/// signal 30 stops it. `resume_with_signal` passes the signal at the breakpoint there and
/// the handler stops at a breakpoint of its own, which is then removed: the handler's later
/// return to `INTERRUPTED` in the frame it interrupted is a pending rendezvous. Returns
/// the breakpoint at `INTERRUPTED` and `HANDLER`'s registers there, for that return.
fn handler_stopped_in_its_handler(
    mock: &MockBackend,
    session: &mut Session,
) -> (ferros_core::BreakpointId, ferros_core::Registers)
{
    use ferros_core::BreakpointRequest;
    use ferros_core::backend::ThreadAction;

    let in_handler = INTERRUPTED + 32;
    let mut registers = session.read_registers_for(HANDLER).expect("registers");
    registers.pc = INTERRUPTED;
    registers.sp = Address::new(0x8000);
    session
        .write_registers_for(HANDLER, &registers)
        .expect("place the handler thread");
    mock.script(
        MockStop::new(StopEvent::Signal {
            thread: HANDLER,
            signal: 30,
        })
        .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("run to the signal");
    assert!(matches!(session.wait(None).expect("wait"), Some(StopEvent::Signal { .. })));
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: INTERRUPTED })
        .expect("breakpoint at the signal pc");
    let handler_bp = session
        .add_breakpoint(BreakpointRequest::Software { address: in_handler })
        .expect("breakpoint in the handler");
    let mut handler_registers = registers.clone();
    handler_registers.pc = in_handler;
    handler_registers.sp = Address::new(0x7000);
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: HANDLER,
            pc: in_handler,
        })
        .set_registers(HANDLER, handler_registers)
        .expect_resume(
            ResumeRequest::all(ThreadAction::Suspend)
                .with_thread(HANDLER, ThreadAction::Continue)
                .with_signal(HANDLER, 30),
        ),
    );
    session.resume_with_signal().expect("pass the signal");
    assert!(matches!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint { pc, .. }) if pc == in_handler
    ));
    session.remove_breakpoint(handler_bp).expect("remove the handler breakpoint");
    (id, registers)
}

fn handler_stopped_process() -> MockBackend
{
    let mut registers = arm64_registers();
    registers.pc = INTERRUPTED + 16;
    MockBackend::new()
        .with_thread(THREAD, registers.clone())
        .with_thread(HANDLER, registers)
        .with_region(INTERRUPTED, "r-x", vec![0; 64])
}

/// #150 re-review: a step over a breakpoint at another address than a stopped handler's
/// rendezvous, refused for a late stop or failing to start, leaves that rendezvous alone:
/// the handler's return to its own breakpoint is still not reported or counted.
#[test]
fn unstarted_step_over_elsewhere_keeps_a_stopped_handlers_rendezvous()
{
    use ferros_core::BreakpointRequest;

    for refused in [true, false] {
        let mock = handler_stopped_process();
        let mut session = Session::new(Box::new(mock.clone()));
        session.launch(&LaunchSpec::new("scripted-step-elsewhere")).expect("launch");
        let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

        // The other thread arrives at a different breakpoint; the step over it runs
        // nothing.
        let elsewhere = INTERRUPTED + 8;
        session
            .add_breakpoint(BreakpointRequest::Software { address: elsewhere })
            .expect("breakpoint elsewhere");
        let mut at_elsewhere = session.read_registers_for(THREAD).expect("registers");
        at_elsewhere.pc = elsewhere;
        session.write_registers_for(THREAD, &at_elsewhere).expect("move");
        if refused {
            let late = StopEvent::Interrupted { thread: Some(THREAD) };
            mock.stop_before_next_resume(std::iter::once(MockStop::new(late.clone())));
            session.resume().expect("a stop raced the step over");
            assert_eq!(session.wait(None).expect("wait"), Some(late));
        } else {
            mock.fail_next(
                MockCallKind::Resume,
                BackendError::Os {
                    code: 5,
                    message: "step failed".into(),
                },
            );
            assert!(session.resume().is_err());
            assert_eq!(session.state(), SessionState::Stopped);
        }
        assert_eq!(mock.peek_memory(elsewhere, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));

        // The other thread is moved off; the handler returns to its breakpoint in the frame
        // it interrupted: the rendezvous, so the continue goes on to the interrupt.
        let mut moved = session.read_registers_for(THREAD).expect("registers");
        moved.pc = INTERRUPTED + 16;
        session.write_registers_for(THREAD, &moved).expect("move off");
        mock.script(
            MockStop::new(StopEvent::Breakpoint {
                thread: HANDLER,
                pc: INTERRUPTED,
            })
            .set_registers(HANDLER, registers)
            .expect_resume(ResumeRequest::continue_all()),
        );
        mock.script(
            MockStop::new(StopEvent::SingleStep { thread: HANDLER })
                .set_pc(HANDLER, INTERRUPTED + 4)
                .expect_resume(ResumeRequest::step(HANDLER)),
        );
        let end = StopEvent::Interrupted { thread: Some(HANDLER) };
        mock.script(MockStop::new(end.clone()).expect_resume(ResumeRequest::continue_all()));
        session.resume().expect("let the handler return");
        assert_eq!(session.wait(None).expect("wait"), Some(end), "refused: {refused}");
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
        assert_eq!(mock.script_len(), 0);
    }
}

/// #150 re-review: another thread's signal stop is reported before the stopped handler's
/// return to its breakpoint, which stays queued. `resume_with_signal` for that signal is
/// deferred for the queued return, so the signal stays carried; the session then consumes
/// the return as the rendezvous and continues every thread in place of that resume. The
/// continue passes the carried signal (its thread runs, so it would lose it otherwise),
/// and it is not carried any more for a later `resume_with_signal`.
#[test]
fn handler_return_continue_passes_a_signal_carried_from_a_deferred_resume()
{
    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("scripted-deferred-signal")).expect("launch");
    let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let signal = StopEvent::Signal {
        thread: THREAD,
        signal: 31,
    };
    mock.script_simultaneous(vec![
        MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()),
        MockStop::new(StopEvent::Breakpoint {
            thread: HANDLER,
            pc: INTERRUPTED,
        })
        .set_registers(HANDLER, registers),
    ]);
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    assert_eq!(mock.pending_len(), 1, "the handler's return is queued");

    let _ = mock.take_calls();
    session.resume_with_signal().expect("deferred for the queued return");
    assert_eq!(resumes(&mock), Vec::<ResumeRequest>::new(), "nothing was resumed");
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: HANDLER })
            .set_pc(HANDLER, INTERRUPTED + 4)
            .expect_resume(ResumeRequest::step(HANDLER)),
    );
    let end = StopEvent::Interrupted { thread: Some(HANDLER) };
    let with_signal = ResumeRequest::continue_all().with_signal(THREAD, 31);
    mock.script(MockStop::new(end.clone()).expect_resume(with_signal.clone()));
    assert_eq!(session.wait(None).expect("wait"), Some(end));
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(HANDLER), with_signal]);
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);

    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.resume_with_signal().expect("nothing is carried any more");
    assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// #150 re-review: a step-over's final continue that passes another thread's signal,
/// refused for a late stop, sent nothing: the next `resume_with_signal` passes that signal.
#[test]
fn refused_step_over_final_continue_keeps_another_threads_signal()
{
    let other = ThreadId(2);
    let mut registers = arm64_registers();
    registers.pc = BREAKPOINT;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers.clone())
        .with_thread(other, registers)
        .with_region(Address::new(0x1_0000_0000), "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()));
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let mut elsewhere = session.read_registers_for(other).expect("registers");
    elsewhere.pc = BREAKPOINT + 16;
    session.write_registers_for(other, &elsewhere).expect("move");
    let signal = StopEvent::Signal {
        thread: other,
        signal: 31,
    };
    mock.script(MockStop::new(signal.clone()).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("run to the signal");
    assert_eq!(session.wait(None).expect("wait"), Some(signal));
    session
        .add_breakpoint(ferros_core::BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint at THREAD's pc");

    // Resume 1 steps THREAD over the breakpoint; the late stop races resume 2, the final
    // continue that passes the other thread's signal.
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    let late = StopEvent::Interrupted { thread: Some(THREAD) };
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(late.clone())));
    session.resume_with_signal().expect("a stop raced the final continue");
    assert_eq!(session.wait(None).expect("wait"), Some(late));

    let _ = mock.take_calls();
    let pass = ResumeRequest::continue_all().with_signal(other, 31);
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(pass.clone()));
    session.resume_with_signal().expect("pass the held signal");
    assert_eq!(resumes(&mock), vec![pass]);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}
