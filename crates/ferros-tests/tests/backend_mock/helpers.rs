//! Shared constants, builders and fixtures for the mock-backend tests.

use ferros_core::backend::mock::{MockBackend, MockCall, MockStop, arm64_registers};
use ferros_core::backend::{ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectImage;
use ferros_core::runtime::ExitSource;
use ferros_core::runtime::mock::MockRuntime;
use ferros_core::types::{Address, ProcessId, ThreadId};
use ferros_core::{Session, SessionState, arch};

pub(crate) const THREAD: ThreadId = ThreadId(1);

pub(crate) const BREAKPOINT: Address = Address::new(0x1_0000_0008);

pub(crate) fn scripted_process() -> MockBackend
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

/// A portable, synthetic arm64 image in the neutral `MockObjectFormat` with two DWARF
/// subprograms named `multi`. No compiler, fixture process, or platform API is needed to
/// resolve multiple sites.
pub(crate) struct MultiSiteImage(pub(crate) std::path::PathBuf);

impl MultiSiteImage
{
    pub(crate) fn new() -> (Self, MockBackend)
    {
        use ferros_core::objfile::SectionId;
        use ferros_core::objfile::mock::MockObjectFile;

        let (mut image, path) = ferros_tests::scratch::create_file("ferros-mock-multi", "");
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

        let file = MockObjectFile::new(IMAGE_BASE.value(), 0x2000)
            .with_dwarf(SectionId::DebugInfo, &info)
            .with_dwarf(SectionId::DebugAbbrev, &abbrev)
            .with_dwarf(SectionId::DebugLine, &[0]);
        std::io::Write::write_all(&mut image, &file.to_bytes()).expect("write synthetic image");
        drop(image);
        // The mapped image: its header, then code (`0x1F` bytes) up to the segment's end.
        let mut bytes = MockObjectImage::new(0..1, 0, 0)
            .with_mapping(IMAGE_BASE.value(), 0x2000)
            .header_bytes();
        bytes.resize(0x2000, 0x1F);
        let mut registers = arm64_registers();
        registers.pc = IMAGE_BASE;
        let mock = MockBackend::new()
            .with_thread(THREAD, registers)
            .with_region(IMAGE_BASE, "r-x", bytes)
            .with_image(ferros_core::backend::LoadedImage {
                path: path.clone(),
                load_address: IMAGE_BASE,
                build_id: ferros_core::types::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
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

pub(crate) const IMAGE_BASE: Address = Address::new(0xFFFF_F000);

pub(crate) const SECOND_SITE: Address = Address::new(0x1_0000_0010);

pub(crate) const STACK: Address = Address::new(0x7_0000_0000);

pub(crate) const CALLEE: Address = Address::new(0x1_0000_0000);

/// A process stopped in a frame whose return address is `BREAKPOINT`: frame pointer and
/// stack pointer at `STACK`, which holds saved fp 0 and saved lr `BREAKPOINT`.
pub(crate) fn returning_to_breakpoint() -> MockBackend
{
    let mut stack = vec![0u8; 32];
    stack[8..16].copy_from_slice(&BREAKPOINT.value().to_le_bytes());
    MockBackend::new()
        .with_thread(THREAD, in_callee(arm64_registers()))
        .with_region(CALLEE, "r-x", vec![0x1F; 64])
        .with_region(STACK, "rw-", stack)
}

pub(crate) fn in_callee(mut registers: ferros_core::Registers) -> ferros_core::Registers
{
    registers.pc = CALLEE;
    registers.fp = STACK;
    registers.sp = STACK;
    registers
}

/// `step_out` from the callee, with the next stop scripted as the return trap.
pub(crate) fn step_out_to_breakpoint(mock: &MockBackend, session: &mut Session)
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

/// `step_out` from the callee, raced by an interrupt: the interrupt is reported first and
/// the return trap's exception stays queued behind it, as on a real kernel when the
/// thread reaches the trap just as it is stopped. The step ends and removes its trap,
/// so the queued stop is left over from a trap that is gone (issue #142). `more` are
/// further stops queued after it.
pub(crate) fn step_out_interrupted_at_return_trap(mock: &MockBackend, session: &mut Session, more: Vec<MockStop>)
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

/// The `Resume` requests `mock` received since its calls were last taken.
pub(crate) fn resumes(mock: &MockBackend) -> Vec<ResumeRequest>
{
    mock.take_calls()
        .into_iter()
        .filter_map(|call| match call {
            MockCall::Resume(request) => Some(request),
            _ => None,
        })
        .collect()
}

/// The thread whose signal handler stops in the `handler_stopped_*` tests.
pub(crate) const HANDLER: ThreadId = ThreadId(2);

/// Where `HANDLER` is interrupted by its signal; a breakpoint is set there.
pub(crate) const INTERRUPTED: Address = Address::new(0x1000);

/// Two threads: `THREAD` at `INTERRUPTED + 16` and `HANDLER` at `INTERRUPTED`, where
/// signal 30 stops it. `resume_with_signal` passes the signal at the breakpoint there and
/// the handler stops at a breakpoint of its own, which is then removed: the handler's later
/// return to `INTERRUPTED` in the frame it interrupted is a pending rendezvous. Returns
/// the breakpoint at `INTERRUPTED` and `HANDLER`'s registers there, for that return.
pub(crate) fn handler_stopped_in_its_handler(
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

pub(crate) fn handler_stopped_process() -> MockBackend
{
    let mut registers = arm64_registers();
    registers.pc = INTERRUPTED + 16;
    MockBackend::new()
        .with_thread(THREAD, registers.clone())
        .with_thread(HANDLER, registers)
        .with_region(INTERRUPTED, "r-x", vec![0; 64])
}

/// The continue `step_out` from `in_callee` issues: only the stepping thread runs.
pub(crate) fn step_out_continue() -> ResumeRequest
{
    use ferros_core::backend::ThreadAction;

    ResumeRequest::all(ThreadAction::Suspend).with_thread(THREAD, ThreadAction::Continue)
}

/// A breakpoint stop of `thread` at `pc`, where no trap is armed: a private trap that an
/// earlier operation removed before the trap's exception was taken (see `stale_trap`).
pub(crate) fn stale_trap_stop(thread: ThreadId, pc: Address) -> MockStop
{
    MockStop::new(StopEvent::Breakpoint { thread, pc })
}

/// `brk #1` in memory order: a trap the program itself contains (Rust emits it for a
/// trap), not one of the session's.
pub(crate) const BRK_1: [u8; 4] = 0xD420_0020_u32.to_le_bytes();

/// Where a peer thread's breakpoint stop is taken in the step-over tests of #158: a
/// stale private trap (no trap at the address), a session breakpoint, or the program's
/// own `brk #1`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PeerTrap
{
    Stale,
    Armed,
    Program,
}

/// The file name of the exit image the tests' runtime names: not a macOS library, so a
/// trap there comes from the runtime the session was given, not from the engine.
pub(crate) const EXIT_IMAGE: &str = "libexits.so.1";

/// The entry symbol of `EXIT_IMAGE` the tests' runtime traps.
pub(crate) const EXIT_ENTRY: &str = "jump_leaf";

/// A runtime whose one exit source is `EXIT_IMAGE`'s `EXIT_ENTRY`.
pub(crate) fn exit_runtime() -> std::sync::Arc<dyn ferros_core::runtime::TargetRuntime>
{
    MockRuntime::new()
        .with_exit_source(ExitSource::named(EXIT_IMAGE, &[EXIT_ENTRY]))
        .into_shared()
}

/// `HANDLER`'s return to its breakpoint at `INTERRUPTED`, in the frame it interrupted.
pub(crate) fn handler_return(registers: &ferros_core::Registers) -> MockStop
{
    MockStop::new(StopEvent::Breakpoint {
        thread: HANDLER,
        pc: INTERRUPTED,
    })
    .set_registers(HANDLER, registers.clone())
}

/// The session's step of `HANDLER` off its breakpoint at `INTERRUPTED`.
pub(crate) fn handler_step_off() -> MockStop
{
    MockStop::new(StopEvent::SingleStep { thread: HANDLER })
        .set_pc(HANDLER, INTERRUPTED + 4)
        .expect_resume(ResumeRequest::step(HANDLER))
}

pub(crate) fn known(bytes: &[u8]) -> ferros_core::BuildIdentity
{
    ferros_core::BuildIdentity::Known(ferros_core::BuildId::new(bytes).expect("build id"))
}

pub(crate) const SIGNAL_THREAD: ThreadId = ThreadId(2);

/// Files removed when a test ends, pass or fail.
pub(crate) struct RemoveOnDrop(pub(crate) Vec<std::path::PathBuf>);

impl Drop for RemoveOnDrop
{
    fn drop(&mut self)
    {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}
