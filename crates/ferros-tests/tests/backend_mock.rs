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
