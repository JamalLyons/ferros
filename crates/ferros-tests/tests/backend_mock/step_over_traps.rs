//! Step-over with a peer thread's trap stop, and a dying target.

use std::time::{Duration, Instant};

use ferros_core::backend::mock::{MockBackend, MockCall, MockStop, arm64_registers};
use ferros_core::backend::{LaunchSpec, LoadedImage, ResumeRequest, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, ThreadId};
use ferros_core::{Backend, BackendResult, BreakpointRequest, Session, SessionState, arch};

use crate::helpers::*;

/// The third thread of `step_over_process`, which takes the peer stop at `PEER_SITE`.
const PEER: ThreadId = ThreadId(3);

const PEER_SITE: Address = Address::new(0x1_0000_0020);

/// `THREAD` at a user breakpoint at `BREAKPOINT`; with `two_sites`, a second thread at
/// another one at `SECOND_SITE`, so `resume` steps over both before its final continue.
/// `PEER` runs elsewhere; `trap` says what is at `PEER_SITE`.
fn step_over_process(two_sites: bool, trap: PeerTrap) -> (MockBackend, Session)
{
    let mut code = vec![0x1F; 64];
    if matches!(trap, PeerTrap::Program) {
        code[0x20..0x24].copy_from_slice(&BRK_1);
    }
    let at = |pc: Address| {
        let mut registers = arm64_registers();
        registers.pc = pc;
        registers
    };
    let second = if two_sites { SECOND_SITE } else { CALLEE + 0x30 };
    let mock = MockBackend::new()
        .with_thread(THREAD, at(BREAKPOINT))
        .with_thread(ThreadId(2), at(second))
        .with_thread(PEER, at(CALLEE + 0x38))
        .with_region(CALLEE, "r-x", code);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let mut sites = vec![BREAKPOINT];
    if two_sites {
        sites.push(SECOND_SITE);
    }
    if matches!(trap, PeerTrap::Armed) {
        sites.push(PEER_SITE);
    }
    for address in sites {
        session
            .add_breakpoint(BreakpointRequest::Software { address })
            .expect("breakpoint");
    }
    let _ = mock.take_calls();
    (mock, session)
}

/// `PEER`'s breakpoint stop at `PEER_SITE`.
fn peer_stop() -> StopEvent
{
    StopEvent::Breakpoint {
        thread: PEER,
        pc: PEER_SITE,
    }
}

const STALE_STEP_DELAY: Duration = Duration::from_millis(20);

/// Simulate time spent waiting for the scripted single-step stop.
struct DelayedStep(MockBackend);

impl Backend for DelayedStep
{
    fn launch(&mut self, spec: &LaunchSpec) -> BackendResult<ProcessId>
    {
        self.0.launch(spec)
    }

    fn attach(&mut self, pid: ProcessId) -> BackendResult<()>
    {
        self.0.attach(pid)
    }

    fn detach(&mut self) -> BackendResult<()>
    {
        self.0.detach()
    }

    fn hold_attached_on_drop(&mut self)
    {
        self.0.hold_attached_on_drop();
    }

    fn kill(&mut self) -> BackendResult<()>
    {
        self.0.kill()
    }

    fn wait(&mut self, timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>
    {
        let event = self.0.wait(timeout)?;
        if matches!(event, Some(StopEvent::SingleStep { .. })) {
            std::thread::sleep(STALE_STEP_DELAY);
        }
        Ok(event)
    }

    fn resume(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        self.0.resume(request)
    }

    fn interrupt(&mut self) -> BackendResult<()>
    {
        self.0.interrupt()
    }

    fn threads(&mut self) -> BackendResult<Vec<ThreadId>>
    {
        self.0.threads()
    }

    fn read_registers(&mut self, thread: ThreadId) -> BackendResult<Registers>
    {
        self.0.read_registers(thread)
    }

    fn write_registers(&mut self, thread: ThreadId, registers: &Registers) -> BackendResult<()>
    {
        self.0.write_registers(thread, registers)
    }

    fn read_memory(&mut self, address: Address, len: usize) -> BackendResult<Vec<u8>>
    {
        self.0.read_memory(address, len)
    }

    fn write_memory(&mut self, address: Address, data: &[u8]) -> BackendResult<()>
    {
        self.0.write_memory(address, data)
    }

    fn memory_regions(&mut self) -> BackendResult<Vec<MemoryRegion>>
    {
        self.0.memory_regions()
    }

    fn images(&mut self) -> BackendResult<Vec<LoadedImage>>
    {
        self.0.images()
    }

    fn architecture(&self) -> Architecture
    {
        self.0.architecture()
    }

    fn arch(&self) -> BackendResult<std::sync::Arc<dyn arch::Arch>>
    {
        self.0.arch()
    }
}

/// #227: a late completion of an abandoned step spends part of the caller's wait budget.
#[test]
fn wait_uses_the_remaining_timeout_after_a_stale_single_step()
{
    let mut registers = arm64_registers();
    registers.pc = BREAKPOINT;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(BREAKPOINT, "r-x", vec![0x1F; 8]);
    let mut session = Session::new(Box::new(DelayedStep(mock.clone())), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    session
        .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint");

    let interrupted = StopEvent::Interrupted { thread: None };
    mock.script_simultaneous(vec![
        MockStop::new(interrupted.clone()).expect_resume(ResumeRequest::step(THREAD)),
        MockStop::new(StopEvent::SingleStep { thread: THREAD }).set_pc(THREAD, BREAKPOINT + 4),
    ]);
    session.resume().expect("resume");
    assert_eq!(session.wait(None).expect("wait for interrupt"), Some(interrupted));
    let _ = mock.take_calls();

    let timeout = Duration::from_secs(1);
    let started = Instant::now();
    assert_eq!(session.wait(Some(timeout)).expect("wait after interrupt"), None);
    let elapsed = started.elapsed();
    let calls = mock.take_calls();
    let [MockCall::Wait(Some(first)), MockCall::Wait(Some(second))] = calls.as_slice() else {
        panic!("expected two timed backend waits, got {calls:?}");
    };
    assert_eq!(*first, timeout);
    assert!(
        (timeout.saturating_sub(elapsed)..=timeout.saturating_sub(STALE_STEP_DELAY)).contains(second),
        "second backend wait received {second:?} after spending at least {STALE_STEP_DELAY:?} of {timeout:?}"
    );
}

/// #158: the stop that completes the internal step off the breakpoint at the pc also
/// reports another thread's breakpoint stop at a private trap that is gone. It belongs
/// to no request: the drain before the final continue drops it, and the program runs on.
#[test]
fn step_over_drops_a_stale_trap_stop_reported_with_its_internal_step()
{
    let (mock, mut session) = step_over_process(false, PeerTrap::Stale);
    mock.script_simultaneous(vec![
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
        stale_trap_stop(PEER, PEER_SITE),
    ]);
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    session.resume().expect("resume");
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::step(THREAD), ResumeRequest::continue_all()],
        "the final continue was issued"
    );
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.breakpoints()[0].hit_count, 0);
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(mock.script_len(), 0);
}

/// #158: the same stop at a real trap (a session breakpoint, or the program's own
/// `brk #1`) is the caller's: it is reported, and the final continue is not issued.
#[test]
fn step_over_reports_a_real_trap_stop_reported_with_its_internal_step()
{
    for trap in [PeerTrap::Armed, PeerTrap::Program] {
        let (mock, mut session) = step_over_process(false, trap);
        mock.script_simultaneous(vec![
            MockStop::new(StopEvent::SingleStep { thread: THREAD })
                .set_pc(THREAD, BREAKPOINT + 4)
                .expect_resume(ResumeRequest::step(THREAD)),
            MockStop::new(peer_stop()).set_pc(PEER, PEER_SITE),
        ]);
        session.resume().expect("resume");
        assert_eq!(
            resumes(&mock),
            vec![ResumeRequest::step(THREAD)],
            "{trap:?}: no final continue"
        );
        assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
        assert_eq!(session.wait(None).expect("wait"), Some(peer_stop()), "{trap:?}");
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(mock.pending_len(), 0);
    }
}

/// #158: `resume` steps two threads over the breakpoints at their pcs, one after the
/// other. Another thread's breakpoint stop at a private trap that is gone arrives just
/// before the second step, so the backend refuses it; the stop belongs to no request, so
/// it is dropped and the step is issued again, and the step-over goes on.
#[test]
fn step_over_drops_a_stale_trap_stop_that_refuses_its_next_step()
{
    let other = ThreadId(2);
    let (mock, mut session) = step_over_process(true, PeerTrap::Stale);
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: other })
            .set_pc(other, SECOND_SITE + 4)
            .expect_resume(ResumeRequest::step(other)),
    );
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    mock.stop_before_nth_resume(2, std::iter::once(stale_trap_stop(PEER, PEER_SITE)));
    session.resume().expect("resume");
    assert_eq!(
        resumes(&mock),
        vec![
            ResumeRequest::step(THREAD),
            ResumeRequest::step(other),
            ResumeRequest::step(other),
            ResumeRequest::continue_all(),
        ],
        "the refused step was issued again, then the final continue"
    );
    for address in [BREAKPOINT, SECOND_SITE] {
        assert_eq!(mock.peek_memory(address, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    }
    assert!(session.breakpoints().iter().all(|info| info.hit_count == 0));
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    assert_eq!(mock.script_len(), 0);
}

/// #158: the same refusal for a stop at a real trap (a session breakpoint, or the
/// program's own `brk #1`) ends the step-over: the stop is reported and the second step
/// is not issued again.
#[test]
fn step_over_reports_a_real_trap_stop_that_refuses_its_next_step()
{
    let other = ThreadId(2);
    for trap in [PeerTrap::Armed, PeerTrap::Program] {
        let (mock, mut session) = step_over_process(true, trap);
        mock.script(
            MockStop::new(StopEvent::SingleStep { thread: THREAD })
                .set_pc(THREAD, BREAKPOINT + 4)
                .expect_resume(ResumeRequest::step(THREAD)),
        );
        mock.stop_before_nth_resume(2, std::iter::once(MockStop::new(peer_stop()).set_pc(PEER, PEER_SITE)));
        session.resume().expect("resume");
        assert_eq!(
            resumes(&mock),
            vec![ResumeRequest::step(THREAD), ResumeRequest::step(other)],
            "{trap:?}: the refused step was not issued again"
        );
        for address in [BREAKPOINT, SECOND_SITE] {
            assert_eq!(mock.peek_memory(address, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
        }
        assert_eq!(session.wait(None).expect("wait"), Some(peer_stop()), "{trap:?}");
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(mock.script_len(), 0);
        assert_eq!(mock.pending_len(), 0);
    }
}

/// Whether `calls` write memory at `address` after their last `Resume`: the cleanup after
/// a refused resume reached the target.
fn writes_after_last_resume(calls: &[MockCall], address: Address) -> bool
{
    let refused = calls
        .iter()
        .rposition(|call| matches!(call, MockCall::Resume(_)))
        .expect("a resume");
    calls[refused + 1..]
        .iter()
        .any(|call| matches!(call, MockCall::WriteMemory { address: written, .. } if *written == address))
}

/// #155: `resume` steps over the breakpoint at the pc, and the backend refuses that step
/// (the first, or the next of two sites) for a stop, after which the target dies before
/// the trap is put back. The failed write does not hide the refusal: `resume` succeeds,
/// and `wait` reports how the target ended.
#[test]
fn refused_step_over_on_a_dying_target_reports_its_death()
{
    let other = ThreadId(2);
    for (two_sites, nth) in [(false, 1), (true, 2)] {
        let (mock, mut session) = step_over_process(two_sites, PeerTrap::Stale);
        if two_sites {
            mock.script(
                MockStop::new(StopEvent::SingleStep { thread: THREAD })
                    .set_pc(THREAD, BREAKPOINT + 4)
                    .expect_resume(ResumeRequest::step(THREAD)),
            );
        }
        let exited = StopEvent::Exited { code: 7 };
        mock.die_refusing_nth_resume(nth, exited.clone(), Duration::from_millis(50));
        session
            .resume()
            .unwrap_or_else(|error| panic!("site {nth}: the refused step reported {error:?}"));
        let site = if two_sites { SECOND_SITE } else { BREAKPOINT };
        let stepped = if two_sites { other } else { THREAD };
        let calls = mock.take_calls();
        assert_eq!(
            calls.iter().rev().find_map(|call| match call {
                MockCall::Resume(request) => Some(request.clone()),
                _ => None,
            }),
            Some(ResumeRequest::step(stepped)),
            "site {nth}: the step was the refused resume"
        );
        assert!(
            writes_after_last_resume(&calls, site),
            "site {nth}: the trap was put back after the refusal"
        );
        assert_eq!(session.state(), SessionState::Running);
        assert_eq!(session.wait(None).expect("wait"), Some(exited.clone()), "site {nth}");
        assert_eq!(session.state(), SessionState::Exited);
        assert_eq!(session.last_stop(), Some(&exited));
    }
}

/// #155: the same death after the backend refuses the continue of a source step to its
/// return trap: releasing the private trap fails, and `wait` still reports the death.
#[test]
fn refused_step_out_continue_on_a_dying_target_reports_its_death()
{
    let mock = returning_to_breakpoint();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let killed = StopEvent::Killed { signal: 9 };
    mock.die_refusing_nth_resume(1, killed.clone(), Duration::from_millis(50));
    let _ = mock.take_calls();
    session
        .step_out()
        .unwrap_or_else(|error| panic!("the refused continue reported {error:?}"));
    let calls = mock.take_calls();
    assert!(
        calls.contains(&MockCall::Resume(step_out_continue())),
        "the continue was the refused resume"
    );
    assert!(
        writes_after_last_resume(&calls, BREAKPOINT),
        "the return trap was released after the refusal"
    );
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.wait(None).expect("wait"), Some(killed.clone()));
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.last_stop(), Some(&killed));
}

/// #155: `wait(Some(Duration::ZERO))` takes the completion of `resume`'s step off the
/// breakpoint at the pc, and the backend refuses the final continue; the target died, and
/// its terminal event comes later. The wait keeps to its budget: it returns `None` at
/// once, the session running, and a later `wait` reports the death.
#[test]
fn zero_timeout_wait_does_not_block_on_a_dying_targets_refused_continue()
{
    let (mock, mut session) = step_over_process(false, PeerTrap::Stale);
    let exited = StopEvent::Exited { code: 0 };
    mock.die_refusing_nth_resume(2, exited.clone(), Duration::from_millis(1500));
    // No completion is scripted yet, so `resume` leaves the step to `wait`.
    session.resume().expect("resume");
    assert_eq!(resumes(&mock), vec![ResumeRequest::step(THREAD)]);
    assert_eq!(session.state(), SessionState::Running);

    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: THREAD })
            .set_pc(THREAD, BREAKPOINT + 4)
            .expect_resume(ResumeRequest::step(THREAD)),
    );
    let started = Instant::now();
    let waited = session.wait(Some(Duration::ZERO));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "wait(Some(ZERO)) blocked for {elapsed:?}, returning {waited:?}"
    );
    assert_eq!(waited.expect("wait"), None);
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::continue_all()],
        "the final continue was refused once"
    );
    assert_eq!(mock.peek_memory(BREAKPOINT, 4), Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec()));
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.wait(None).expect("wait"), Some(exited.clone()));
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.last_stop(), Some(&exited));
}

/// #155 review: a stopped handler's return to its breakpoint is consumed inside `wait`, and
/// the session reissues the user's continue, which steps the handler off the breakpoint
/// first. The backend refuses that step: the target died, so putting the trap back fails,
/// and its terminal event comes later. That internal resume goes through `refusal`,
/// which now behaves like the other refused resumes: `wait(Some(ZERO))` neither returns
/// the write error nor blocks, the session runs on, and a later `wait` reports the death.
#[test]
fn zero_timeout_wait_reports_a_dying_targets_refused_handler_continue_later()
{
    let mock = handler_stopped_process();
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("scripted-dying-handler")).expect("launch");
    let (_, registers) = handler_stopped_in_its_handler(&mock, &mut session);

    session.resume().expect("let the handler return");
    let exited = StopEvent::Exited { code: 5 };
    mock.die_refusing_nth_resume(1, exited.clone(), Duration::from_millis(1500));
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: HANDLER,
            pc: INTERRUPTED,
        })
        .set_registers(HANDLER, registers),
    );
    let _ = mock.take_calls();
    let started = Instant::now();
    let waited = session.wait(Some(Duration::ZERO));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "wait(Some(ZERO)) blocked for {elapsed:?}, returning {waited:?}"
    );
    assert_eq!(waited.expect("the refused step off the rendezvous"), None);
    assert_eq!(
        resumes(&mock),
        vec![ResumeRequest::step(HANDLER)],
        "the step off the rendezvous trap was the refused resume"
    );
    assert_eq!(session.state(), SessionState::Running);
    assert_eq!(session.wait(None).expect("wait"), Some(exited.clone()));
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.last_stop(), Some(&exited));
}
