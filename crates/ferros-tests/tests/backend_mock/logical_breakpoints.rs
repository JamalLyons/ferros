//! Logical breakpoints with several sites: rollback on failure, detach and re-arm.

use ferros_core::backend::mock::{MockBackend, MockCallKind, MockState, MockStop};
use ferros_core::backend::{BackendError, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::{BreakpointLocation, LogicalBreakpointId, Session, SessionState, arch};

use crate::helpers::*;

fn multi_site_session() -> (MultiSiteImage, MockBackend, Session, LogicalBreakpointId)
{
    let (image, mock) = MultiSiteImage::new();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
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
