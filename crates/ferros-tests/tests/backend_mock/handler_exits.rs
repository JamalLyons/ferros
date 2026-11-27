//! Signal handlers: return, long-jump exit and replacement exit images.

use ferros_core::backend::mock::{MockBackend, MockCallKind, MockStop};
use ferros_core::backend::{BackendError, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::{MockObjectFormat, MockObjectImage};
use ferros_core::runtime::ExitSource;
use ferros_core::runtime::mock::MockRuntime;
use ferros_core::types::Address;
use ferros_core::{BreakpointKind, BreakpointLocation, DebuggerError, Session, SessionState, arch};

use crate::helpers::*;

/// A session on `mock` with the neutral format and `exit_runtime`.
fn exit_session(mock: &MockBackend) -> Session
{
    Session::new(Box::new(mock.clone()), MockObjectFormat::shared()).with_runtime(exit_runtime())
}

/// Where `EXIT_IMAGE` is mapped in `handler_stopped_with_long_jump`: its file addresses
/// are its runtime ones (no slide).
const LONG_JUMP_IMAGE: Address = Address::new(0x2_0000_0000);

/// `EXIT_ENTRY` in that image, where the session traps a stopped handler's exit.
const LONG_JUMP: Address = Address::new(0x2_0000_1000);

const SECOND_EXIT_ENTRY: &str = "second_leaf";
const SECOND_EXIT: Address = Address::new(0x2_0000_1004);

/// A mapped `EXIT_IMAGE` with `EXIT_ENTRY` at `LONG_JUMP` plus a second exit, in the neutral
/// `MockObjectFormat`: its header at the load address, code at 0x1000..0x1100, its table
/// at 0x4000 and names at 0x5000.
fn long_jump_image() -> Vec<u8>
{
    let base = LONG_JUMP_IMAGE.value();
    let image = MockObjectImage::new(LONG_JUMP.value()..LONG_JUMP.value() + 0x100, base + 0x4000, base + 0x5000)
        .with_symbol(LONG_JUMP.value(), EXIT_ENTRY)
        .with_symbol(SECOND_EXIT.value(), SECOND_EXIT_ENTRY);
    let mut bytes = image.header_bytes();
    bytes.resize(0x1000, 0);
    bytes.extend(std::iter::repeat_n(0x1F, 0x3000));
    bytes.extend(image.table_bytes());
    bytes.resize(0x5000, 0);
    bytes.extend(image.name_bytes());
    bytes.resize(0x6000, 0);
    bytes
}

/// `handler_stopped_process` with `EXIT_IMAGE` loaded, so the continue that lets a
/// stopped handler run on traps `EXIT_ENTRY` for its thread (with `exit_session`).
fn handler_stopped_with_long_jump() -> MockBackend
{
    handler_stopped_process()
        .with_region(LONG_JUMP_IMAGE, "r-x", long_jump_image())
        .with_image(ferros_core::backend::LoadedImage {
            path: format!("/usr/lib/{EXIT_IMAGE}").into(),
            load_address: LONG_JUMP_IMAGE,
            build_id: ferros_core::types::BuildIdentity::Unknown,
            role: ferros_core::backend::ImageRole::Library,
        })
}

/// Where `THREAD`'s breakpoint stop is taken in the handler-return tests of #165, inside
/// `handler_stopped_process`'s code region and away from every breakpoint it sets.
const HANDLER_PEER_SITE: Address = Address::new(0x1008);

/// `THREAD`'s breakpoint stop at `HANDLER_PEER_SITE`, with `trap` there: none (a private
/// trap that is gone), a session breakpoint (added here), or the program's `brk #1`.
fn handler_peer_stop(session: &mut Session, trap: PeerTrap) -> MockStop
{
    let stop = MockStop::new(StopEvent::Breakpoint {
        thread: THREAD,
        pc: HANDLER_PEER_SITE,
    })
    .set_pc(THREAD, HANDLER_PEER_SITE);
    match trap {
        PeerTrap::Stale => stop,
        PeerTrap::Armed => {
            session
                .add_breakpoint(ferros_core::BreakpointRequest::Software {
                    address: HANDLER_PEER_SITE,
                })
                .expect("breakpoint at the peer's stop");
            stop
        }
        PeerTrap::Program => stop.write_memory(HANDLER_PEER_SITE, BRK_1),
    }
}

/// #165: a stopped handler's return to its rendezvous is reported together with
/// another thread's breakpoint stop at a private trap that is gone. That stop belongs to
/// no request: it is dropped, and the user's continue is reissued.
#[test]
fn handler_return_drops_a_stale_trap_stop_reported_with_it()
{
    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-stale-handler-return"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let peer = handler_peer_stop(&mut session, PeerTrap::Stale);
    mock.script_simultaneous(vec![
        handler_return(&registers).expect_resume(ResumeRequest::continue_all()),
        peer,
    ]);
    mock.script(handler_step_off());
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::continue_all(),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all()
        ],
        "the user's continue was reissued"
    );
    assert_eq!(mock.script_len(), 0);
}

/// #165: the same stop at a real trap (a session breakpoint, or the program's own
/// `brk #1`) is the caller's: it is reported, and the continue is not reissued.
#[test]
fn handler_return_reports_a_real_trap_stop_reported_with_it()
{
    for trap in [PeerTrap::Armed, PeerTrap::Program] {
        let mock = handler_stopped_process();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        session
            .launch(&LaunchSpec::new("scripted-real-handler-return"))
            .expect("launch");
        let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

        let peer = handler_peer_stop(&mut session, trap);
        let reported = peer.event().clone();
        mock.script_simultaneous(vec![
            handler_return(&registers).expect_resume(ResumeRequest::continue_all()),
            peer,
        ]);
        let _ = mock.take_calls();
        session.resume().expect("let the handler return");
        assert_eq!(session.wait(None).expect("wait"), Some(reported), "{trap:?}");
        assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()], "{trap:?}");
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
        assert_eq!(mock.pending_len(), 0);
    }
}

/// A peer stop is reported before a queued handler return. Selecting a caller and polling
/// `wait` lets that return resume the process: the session is running, the selected frame
/// is the innermost one, and another selection is refused.
#[test]
fn queued_handler_return_resumes_and_clears_the_selected_frame()
{
    let stack = Address::new(0x8000);
    let mut bytes = vec![0u8; 16];
    bytes[8..].copy_from_slice(&(INTERRUPTED + 24).value().to_le_bytes());
    let mock = handler_stopped_process().with_region(stack, "rw-", bytes);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-queued-handler-return"))
        .expect("launch");
    let mut peer_registers = session.read_registers_for(THREAD).expect("peer registers");
    peer_registers.fp = stack;
    peer_registers.sp = stack;
    session
        .write_registers_for(THREAD, &peer_registers)
        .expect("give the peer a caller frame");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let peer = handler_peer_stop(&mut session, PeerTrap::Armed);
    mock.script_simultaneous(vec![
        peer.expect_resume(ResumeRequest::continue_all()),
        handler_return(&registers).expect_resume(ResumeRequest::continue_all()),
    ]);
    session.resume().expect("continue while the handler is stopped");
    let peer_stop = session.wait(None).expect("wait").expect("the peer stop");
    assert!(
        matches!(peer_stop, StopEvent::Breakpoint { thread: THREAD, .. }),
        "{peer_stop:?}"
    );
    assert_eq!(session.state(), SessionState::Stopped);

    session.select_frame(1).expect("outer frame");
    assert_eq!(session.selected_frame(), 1);
    assert_eq!(session.wait(Some(std::time::Duration::ZERO)).expect("poll"), None);
    assert_eq!(session.selected_frame(), 0);
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.last_stop().is_none());
    assert!(matches!(session.select_frame(0), Err(DebuggerError::NotStopped)));
}

/// #165: the reissued continue after a stopped handler's return steps the handler off its
/// breakpoint first; another thread's breakpoint stop at a private trap that is gone
/// arrives just before that step, so the backend refuses it. The stop belongs to no
/// request: it is dropped and the continue started again.
#[test]
fn handler_continue_drops_a_stale_trap_stop_that_refuses_it()
{
    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-stale-handler-refusal"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let peer = handler_peer_stop(&mut session, PeerTrap::Stale);
    mock.script(handler_return(&registers).expect_resume(ResumeRequest::continue_all()));
    mock.stop_before_nth_resume(2, std::iter::once(peer));
    mock.script(handler_step_off());
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::continue_all(),
            ResumeRequest::step(HANDLER),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all()
        ],
        "the refused step was issued again, then the continue"
    );
    assert_eq!(mock.peek_memory(INTERRUPTED, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(mock.script_len(), 0);
}

/// #165: the same refusal for a stop at a real trap (a session breakpoint, or the
/// program's own `brk #1`) is reported, and the step is not issued again.
#[test]
fn handler_continue_reports_a_real_trap_stop_that_refuses_it()
{
    for trap in [PeerTrap::Armed, PeerTrap::Program] {
        let mock = handler_stopped_process();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        session
            .launch(&LaunchSpec::new("scripted-real-handler-refusal"))
            .expect("launch");
        let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

        let peer = handler_peer_stop(&mut session, trap);
        let reported = peer.event().clone();
        mock.script(handler_return(&registers).expect_resume(ResumeRequest::continue_all()));
        mock.stop_before_nth_resume(2, std::iter::once(peer));
        let _ = mock.take_calls();
        session.resume().expect("let the handler return");
        assert_eq!(session.wait(None).expect("wait"), Some(reported), "{trap:?}");
        assert_eq!(
            resumes(&mock),
            vec![ResumeRequest::continue_all(), ResumeRequest::step(HANDLER)],
            "{trap:?}: the refused step was not issued again"
        );
        assert_eq!(mock.peek_memory(INTERRUPTED, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
        assert_eq!(mock.script_len(), 0);
        assert_eq!(mock.pending_len(), 0);
    }
}

/// `HANDLER` at `EXIT_ENTRY`'s exit trap, inside its handler's frame.
fn handler_at_long_jump(registers: &ferros_core::Registers) -> MockStop
{
    let mut at_entry = registers.clone();
    at_entry.pc = LONG_JUMP;
    at_entry.sp = Address::new(0x6F00);
    MockStop::new(StopEvent::Breakpoint {
        thread: HANDLER,
        pc: LONG_JUMP,
    })
    .set_registers(HANDLER, at_entry)
}

/// #165: a stopped handler, let run on, hits the `EXIT_ENTRY` exit trap armed for its
/// continue, together with another thread's breakpoint stop at a private trap that is
/// gone. That stop is dropped and the exit followed: the handler is stepped out of
/// `EXIT_ENTRY` to a landing still inside its own frame, so the rendezvous is kept and the
/// user's continue reissued.
#[test]
fn handler_exit_drops_a_stale_trap_stop_reported_with_it()
{
    let mock = handler_stopped_with_long_jump();
    let mut session = exit_session(&mock);
    session
        .launch(&LaunchSpec::new("scripted-stale-handler-exit"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let peer = handler_peer_stop(&mut session, PeerTrap::Stale);
    mock.script_simultaneous(vec![
        handler_at_long_jump(&registers).expect_resume(ResumeRequest::continue_all()),
        peer,
    ]);
    let mut landed = registers.clone();
    landed.pc = INTERRUPTED + 40;
    landed.sp = Address::new(0x7F00);
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: HANDLER })
            .set_registers(HANDLER, landed)
            .expect_resume(ResumeRequest::step(HANDLER)),
    );
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("let the handler run on");
    assert_eq!(
        mock.peek_memory(LONG_JUMP, 4),
        Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()),
        "the exit is trapped"
    );
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::continue_all(),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all()
        ],
        "the exit was followed, then the continue reissued"
    );
    assert_eq!(mock.script_len(), 0);
}

/// #165: the same stop at a real trap (a session breakpoint, or the program's own
/// `brk #1`) is reported with the exit trap's stop, and the exit is not followed.
#[test]
fn handler_exit_reports_a_real_trap_stop_reported_with_it()
{
    for trap in [PeerTrap::Armed, PeerTrap::Program] {
        let mock = handler_stopped_with_long_jump();
        let mut session = exit_session(&mock);
        session
            .launch(&LaunchSpec::new("scripted-real-handler-exit"))
            .expect("launch");
        let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

        let peer = handler_peer_stop(&mut session, trap);
        let reported = peer.event().clone();
        mock.script_simultaneous(vec![
            handler_at_long_jump(&registers).expect_resume(ResumeRequest::continue_all()),
            peer,
        ]);
        let _ = mock.take_calls();
        session.resume().expect("let the handler run on");
        assert_eq!(session.wait(None).expect("wait"), Some(reported), "{trap:?}");
        assert_eq!(resumes(&mock), vec![ResumeRequest::continue_all()], "{trap:?}");
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
        assert_eq!(mock.pending_len(), 0);
    }
}

/// #165: following a stopped handler's exit steps it out of `EXIT_ENTRY`; another
/// thread's breakpoint stop at a private trap that is gone arrives just before that
/// step, so the backend refuses it. The stop is dropped and the step issued again; the
/// exit lands inside the handler's frame and the user's continue is reissued.
#[test]
fn handler_exit_step_drops_a_stale_trap_stop_that_refuses_it()
{
    let mock = handler_stopped_with_long_jump();
    let mut session = exit_session(&mock);
    session.launch(&LaunchSpec::new("scripted-stale-exit-step")).expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let peer = handler_peer_stop(&mut session, PeerTrap::Stale);
    mock.script(handler_at_long_jump(&registers).expect_resume(ResumeRequest::continue_all()));
    mock.stop_before_nth_resume(2, std::iter::once(peer));
    let mut landed = registers.clone();
    landed.pc = INTERRUPTED + 40;
    landed.sp = Address::new(0x7F00);
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: HANDLER })
            .set_registers(HANDLER, landed)
            .expect_resume(ResumeRequest::step(HANDLER)),
    );
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("let the handler run on");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::continue_all(),
            ResumeRequest::step(HANDLER),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all()
        ],
        "the refused exit step was issued again, then the continue"
    );
    assert_eq!(mock.script_len(), 0);
}

/// #165: the same refusal for a stop at a real trap (a session breakpoint, or the
/// program's own `brk #1`) is reported, and the exit step is not issued again.
#[test]
fn handler_exit_step_reports_a_real_trap_stop_that_refuses_it()
{
    for trap in [PeerTrap::Armed, PeerTrap::Program] {
        let mock = handler_stopped_with_long_jump();
        let mut session = exit_session(&mock);
        session.launch(&LaunchSpec::new("scripted-real-exit-step")).expect("launch");
        let (id, registers) = handler_stopped_in_its_handler(&mock, &mut session);

        let peer = handler_peer_stop(&mut session, trap);
        let reported = peer.event().clone();
        mock.script(handler_at_long_jump(&registers).expect_resume(ResumeRequest::continue_all()));
        mock.stop_before_nth_resume(2, std::iter::once(peer));
        let _ = mock.take_calls();
        session.resume().expect("let the handler run on");
        assert_eq!(session.wait(None).expect("wait"), Some(reported), "{trap:?}");
        assert_eq!(
            resumes(&mock),
            vec![ResumeRequest::continue_all(), ResumeRequest::step(HANDLER)],
            "{trap:?}: the refused exit step was not issued again"
        );
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 0);
        assert_eq!(mock.script_len(), 0);
        assert_eq!(mock.pending_len(), 0);
    }
}

/// #165 review: `resume` steps `THREAD` over a breakpoint, and the step is given up for
/// the stopped handler's return to its rendezvous, so the step's completion comes late.
/// The session consumes the return and reissues the continue, whose step of the handler
/// off its breakpoint is refused for that late completion. It belongs to no request: it is
/// dropped, the step issued again and the continue goes on. (Before, the refusal was
/// returned from `resume` as a `StopPending` error.)
#[test]
fn handler_continue_drops_an_abandoned_steps_completion_that_refuses_it()
{
    use ferros_core::BreakpointRequest;

    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-late-step-handler"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let elsewhere = INTERRUPTED + 8;
    session
        .add_breakpoint(BreakpointRequest::Software { address: elsewhere })
        .expect("breakpoint elsewhere");
    let mut at_elsewhere = session.read_registers_for(THREAD).expect("registers");
    at_elsewhere.pc = elsewhere;
    session.write_registers_for(THREAD, &at_elsewhere).expect("move");

    // The step of `THREAD` runs, but the handler's return is reported first.
    mock.script(
        handler_return(&registers)
            .set_pc(THREAD, elsewhere + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(StopEvent::SingleStep { thread: THREAD })));
    mock.script(handler_step_off());
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("let the handler return");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::step(THREAD),
            ResumeRequest::step(HANDLER),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all()
        ],
        "the refused step was issued again, then the continue"
    );
    assert_eq!(mock.script_len(), 0);
}

/// #167: a handler return cuts short a peer's step before it executes the instruction.
/// The reissued continue starts a fresh step of the peer at the same breakpoint.
#[test]
fn handler_continue_consumes_a_fresh_step_of_the_previously_stepped_thread()
{
    use ferros_core::BreakpointRequest;

    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session
        .launch(&LaunchSpec::new("scripted-handler-fresh-step"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    let address = HANDLER_PEER_SITE;
    session
        .add_breakpoint(BreakpointRequest::Software { address })
        .expect("peer breakpoint");
    let peer_breakpoint = StopEvent::Breakpoint {
        thread: THREAD,
        pc: address,
    };
    mock.script(
        MockStop::new(peer_breakpoint.clone())
            .set_pc(THREAD, address)
            .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("run both threads");
    assert_eq!(session.wait(None).expect("wait"), Some(peer_breakpoint));

    // The handler's in-flight return stop arrives after the peer's first step starts.
    // The peer is still on its breakpoint, so the reissued continue steps it again.
    mock.script(handler_return(&registers).expect_resume(ResumeRequest::step(THREAD)));
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, address + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    mock.script(handler_step_off());
    let exited = StopEvent::Exited { code: 0 };
    mock.script(MockStop::new(exited.clone()).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("complete the fresh step after the handler returns");
    assert_eq!(session.wait(None).expect("wait through both step-overs"), Some(exited));
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::step(THREAD),
            ResumeRequest::step(THREAD),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all(),
        ],
    );
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(mock.script_len(), 0);
}

#[test]
fn step_out_arms_the_replacement_exit_images_entry()
{
    use ferros_core::backend::LoadedImage;

    const BASE: u64 = 0x2_0000_0000;
    const FIRST_ENTRY: u64 = BASE + 0x400;
    const SECOND_ENTRY: u64 = BASE + 0x408;
    const TABLE: u64 = BASE + 0x1000;
    let symbols =
        MockObjectImage::new(FIRST_ENTRY..FIRST_ENTRY + 16, TABLE, TABLE + 0x100).with_symbol(FIRST_ENTRY, EXIT_ENTRY);
    let mut image = symbols.header_bytes();
    image.resize(0x1000, 0);
    image.extend(symbols.table_bytes());
    image.resize(0x1100, 0);
    image.extend(symbols.name_bytes());

    let loaded = |uuid: Option<[u8; 16]>| LoadedImage {
        path: format!("/usr/lib/{EXIT_IMAGE}").into(),
        load_address: Address::new(BASE),
        build_id: uuid.map(ferros_core::types::BuildId::from).into(),
        role: ferros_core::backend::ImageRole::Library,
    };
    let mock = returning_to_breakpoint()
        .with_region(Address::new(BASE), "r-x", image)
        .with_image(loaded(Some([1; 16])));
    let mut session = exit_session(&mock);
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let trap = arch::Arm64::TRAP_INSTRUCTION.to_vec();
    step_out_to_breakpoint(&mock, &mut session);
    assert_eq!(mock.peek_memory(Address::new(FIRST_ENTRY), 4), Ok(trap.clone()));
    assert_eq!(
        session.wait(None).expect("first return"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );

    // The table's first word is the symbol's address.
    mock.overwrite_memory(Address::new(TABLE), &SECOND_ENTRY.to_le_bytes());
    mock.set_images(vec![loaded(None)]);
    let callee = in_callee(session.read_registers_for(THREAD).expect("read registers"));
    session.write_registers_for(THREAD, &callee).expect("return to callee");
    step_out_to_breakpoint(&mock, &mut session);
    assert_eq!(mock.peek_memory(Address::new(FIRST_ENTRY), 4), Ok(trap.clone()));
    assert_eq!(
        session.wait(None).expect("second return"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );

    mock.set_images(vec![loaded(Some([2; 16]))]);
    session.write_registers_for(THREAD, &callee).expect("return to callee");
    step_out_to_breakpoint(&mock, &mut session);
    assert_eq!(
        mock.peek_memory(Address::new(SECOND_ENTRY), 4),
        Ok(trap),
        "a different UUID must move the trap to the replacement entry"
    );
    assert_eq!(mock.peek_memory(Address::new(FIRST_ENTRY), 4), Ok(vec![0; 4]));
    assert_eq!(
        session.wait(None).expect("third return"),
        Some(StopEvent::SingleStep { thread: THREAD })
    );
}

/// #177: a stopped handler's thread sits on a breakpoint of its own, so the user's
/// continue steps it over that breakpoint with the `EXIT_ENTRY` exit trap armed. The exit
/// trap is reported instead of that step's completion, which abandons the step: its
/// thread is named by `stale_step`. Following the exit single-steps that same thread, and
/// the session must take that fresh `SingleStep` as the exit step's completion instead of
/// dropping it as the abandoned step's.
#[test]
fn handler_exit_step_consumes_a_fresh_step_of_the_previously_stepped_thread()
{
    use ferros_core::BreakpointRequest;

    let mock = handler_stopped_with_long_jump();
    let mut session = exit_session(&mock);
    session.launch(&LaunchSpec::new("scripted-exit-fresh-step")).expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    // The handler is stopped at a breakpoint that stays: the continue steps it off first.
    session
        .add_breakpoint(BreakpointRequest::Software {
            address: INTERRUPTED + 32,
        })
        .expect("breakpoint at the handler's stop");

    // The handler's step is cut short by its hit of the exit trap, so that step is
    // abandoned; the followed exit steps the handler out of `EXIT_ENTRY` to a landing
    // inside its own frame, and the continue is reissued.
    mock.script(handler_at_long_jump(&registers).expect_resume(ResumeRequest::step(HANDLER)));
    let mut landed = registers.clone();
    landed.pc = INTERRUPTED + 40;
    landed.sp = Address::new(0x7F00);
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: HANDLER })
            .set_registers(HANDLER, landed)
            .expect_resume(ResumeRequest::step(HANDLER)),
    );
    let exited = StopEvent::Exited { code: 0 };
    mock.script(MockStop::new(exited.clone()).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("let the handler run on");
    assert_eq!(
        session.wait(None).expect("wait through the exit step"),
        Some(exited),
        "the exit step's completion was not dropped"
    );
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::step(HANDLER),
            ResumeRequest::step(HANDLER),
            ResumeRequest::continue_all(),
        ],
    );
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(mock.script_len(), 0);
}

fn exit_restore_error() -> BackendError
{
    BackendError::Os {
        code: 5,
        message: "injected exit hook restore failure".into(),
    }
}

/// The handler returned but every attempt to remove its exit trap failed.
fn handler_return_with_failed_exit_restore() -> (MockBackend, Session, Vec<u8>)
{
    let mock = handler_stopped_with_long_jump();
    let mut session = exit_session(&mock);
    session
        .launch(&LaunchSpec::new("scripted-handler-exit-release"))
        .expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);
    let original = mock.peek_memory(LONG_JUMP, 4).expect("original bytes");

    mock.script(handler_return(&registers).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("let the handler return");
    let trap = arch::Arm64::TRAP_INSTRUCTION.to_vec();
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(trap.clone()), "the exit is trapped");

    // The rendezvous releases the exit trap, and the cleanup after that error may try
    // again: fail each restore until the wait returns.
    let error = exit_restore_error();
    for _ in 0..2 {
        mock.fail_next(MockCallKind::WriteMemory, error.clone());
    }
    assert!(matches!(session.wait(None), Err(DebuggerError::Backend(actual)) if actual == error));
    mock.clear_failures();
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(trap), "the restore failed");
    (mock, session, original)
}

/// #339: the failed restore leaves the exit hook owned by the session. The next resume
/// restores it before running the process.
#[test]
fn resume_retries_a_failed_handler_exit_hook_release()
{
    let (mock, mut session, original) = handler_return_with_failed_exit_restore();

    mock.script(handler_step_off());
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let _ = mock.take_calls();
    session.resume().expect("retry the exit trap's release, then resume");
    assert_eq!(
        mock.peek_memory(LONG_JUMP, 4),
        Ok(original),
        "the exit trap was released before the process ran"
    );
    assert!(
        session
            .breakpoints()
            .iter()
            .all(|info| info.address != LONG_JUMP || info.kind != BreakpointKind::Software),
        "the private exit trap left the table"
    );
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::step(HANDLER), ResumeRequest::continue_all()]
    );
    assert_eq!(mock.script_len(), 0);
}

/// The peer reached the private exit hook as the handler was interrupted. The next
/// source step restores the hook before classifying that queued stop.
#[test]
fn source_step_drops_queued_stop_at_handler_hook_pending_cleanup()
{
    let mock = handler_stopped_with_long_jump();
    let mut session = exit_session(&mock);
    session.launch(&LaunchSpec::new("queued-exit-hook-stop")).expect("launch");
    handler_stopped_in_its_handler(&mock, &mut session);
    let original = mock.peek_memory(LONG_JUMP, 4).expect("original bytes");

    let queued = StopEvent::Breakpoint {
        thread: THREAD,
        pc: LONG_JUMP,
    };
    mock.script_simultaneous(vec![
        MockStop::new(StopEvent::Interrupted { thread: Some(HANDLER) }).expect_resume(ResumeRequest::continue_all()),
        MockStop::new(queued).set_pc(THREAD, LONG_JUMP),
    ]);
    session.resume().expect("run the handler");
    let error = exit_restore_error();
    mock.fail_next(MockCallKind::WriteMemory, error.clone());
    assert!(matches!(session.wait(None), Err(DebuggerError::Backend(actual)) if actual == error));
    assert_eq!(mock.pending_len(), 1);

    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: HANDLER })
            .set_pc(HANDLER, INTERRUPTED + 36)
            .expect_resume(ResumeRequest::step(HANDLER)),
    );
    let _ = mock.take_calls();
    session.step_over().expect("restore the exit hook and step");
    assert_eq!(
        session.wait(None).expect("report the source step"),
        Some(StopEvent::SingleStep { thread: HANDLER })
    );
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(HANDLER)]);
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(original));
    assert_eq!(mock.peek_registers(THREAD).expect("peer registers").pc, LONG_JUMP);
    assert_eq!(mock.pending_len(), 0);
}

#[test]
fn handler_exit_cleanup_preserves_a_new_low_level_breakpoint()
{
    use ferros_core::BreakpointRequest;

    let (mock, mut session, original) = handler_return_with_failed_exit_restore();
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: LONG_JUMP })
        .expect("claim the existing exit hook");
    let hit = StopEvent::Breakpoint {
        thread: THREAD,
        pc: LONG_JUMP,
    };
    mock.script(handler_step_off());
    mock.script(
        MockStop::new(hit.clone())
            .set_pc(THREAD, LONG_JUMP)
            .expect_resume(ResumeRequest::continue_all()),
    );
    session.resume().expect("retry cleanup before continuing");

    assert!(session.breakpoint_info(id).expect("claimed breakpoint survived").enabled);
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.wait(None).expect("report the breakpoint"), Some(hit));
    assert_eq!(session.breakpoint_info(id).expect("claimed breakpoint").hit_count, 1);
    session.remove_breakpoint(id).expect("remove the claimed breakpoint");
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(original));
}

#[test]
fn handler_exit_cleanup_preserves_a_new_logical_breakpoint()
{
    let (mock, mut session, _) = handler_return_with_failed_exit_restore();
    let logical = session
        .set_breakpoint(BreakpointLocation::Address(LONG_JUMP))
        .expect("claim the existing trap");
    mock.script(handler_step_off());
    session.resume().expect("retry cleanup before continuing");

    assert_eq!(session.logical_breakpoint_at(LONG_JUMP).map(|bp| bp.id), Some(logical));
    assert_eq!(
        mock.peek_memory(LONG_JUMP, 4),
        Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()),
        "the new logical breakpoint still traps"
    );
}

#[test]
fn resume_retries_failed_rollback_of_partially_armed_handler_exit_hooks()
{
    let mock = handler_stopped_with_long_jump();
    let runtime = MockRuntime::new()
        .with_exit_source(ExitSource::named(EXIT_IMAGE, &[EXIT_ENTRY, SECOND_EXIT_ENTRY]))
        .into_shared();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared()).with_runtime(runtime);
    session.launch(&LaunchSpec::new("partial-exit-hooks")).expect("launch");
    let (rendezvous, _) = handler_stopped_in_its_handler(&mock, &mut session);
    let original = mock.peek_memory(LONG_JUMP, 4).expect("original first entry");
    let second = mock.peek_memory(SECOND_EXIT, 4).expect("original second entry");

    let arm_error = BackendError::Os {
        code: 5,
        message: "injected second hook insertion failure".into(),
    };
    mock.fail_on_nth(MockCallKind::WriteMemory, 2, arm_error.clone());
    mock.fail_next(MockCallKind::WriteMemory, exit_restore_error());
    let _ = mock.take_calls();
    assert!(matches!(session.resume(), Err(DebuggerError::Backend(actual)) if actual == arm_error));
    assert_eq!(resumes(&mock), [], "failed arming never resumed the target");
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.peek_memory(SECOND_EXIT, 4), Ok(second));
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));

    // Give up the rendezvous so the next continue needs no new exit hooks.
    session
        .remove_breakpoint(rendezvous)
        .expect("remove the handler's return trap");
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("retry the failed rollback before continuing");
    assert_eq!(mock.peek_memory(LONG_JUMP, 4), Ok(original), "the first hook was restored");
    assert!(session.breakpoints().is_empty());
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}
