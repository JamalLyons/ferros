//! The portable `Backend` trait and its in-memory `MockBackend`, used from outside
//! `ferros-core` through the public API only (the `test-util` feature).
//!
//! Unlike every other file here, these tests launch no real process: the mock is a
//! scripted, in-memory backend. They therefore need no `harness::run`, no fixtures and no
//! debugger entitlement, and they are not limited to macOS.

use ferros_core::arch;
use ferros_core::backend::mock::{MockBackend, MockCall, MockState, MockStop, arm64_registers};
use ferros_core::backend::{Backend, BackendResult, LaunchSpec, ResumeRequest, StopEvent};
use ferros_core::types::{Address, ProcessId, ThreadId};

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
