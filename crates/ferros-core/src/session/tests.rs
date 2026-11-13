//! Unit tests for [`Session`] on the scripted [`MockBackend`].

use std::time::Duration;

use super::*;
use crate::arch::Arm64;
use crate::backend::mock::{MockBackend, MockCall, MockCallKind, MockState, MockStop, arm64_registers};
use crate::backend::{BackendError, LaunchSpec, LoadedImage, ResumeRequest, ThreadAction};
use crate::breakpoints::{BreakpointId, BreakpointRequest, BreakpointState, WatchpointAccess};

const T1: ThreadId = ThreadId(1);
const T2: ThreadId = ThreadId(2);
const CODE: u64 = 0x1000;
const BP: u64 = 0x1008;
const BP2: u64 = 0x1010;
const TRAP: [u8; 4] = Arm64::TRAP_INSTRUCTION;

fn regs_at(pc: u64) -> Registers
{
    let mut registers = arm64_registers();
    registers.pc = Address::new(pc);
    registers
}

/// A mock process with two threads, 32 bytes of code (bytes 0..32) and one image.
fn process() -> MockBackend
{
    MockBackend::new()
        .with_pid(ProcessId(42))
        .with_thread(T1, regs_at(CODE))
        .with_thread(T2, regs_at(CODE + 4))
        .with_region(Address::new(CODE), "r-x", (0u8..32).collect::<Vec<_>>())
}

fn spec() -> LaunchSpec
{
    LaunchSpec::new("/bin/prog")
}

fn session(mock: &MockBackend) -> Session
{
    Session::new(Box::new(mock.clone()))
}

fn launched(mock: &MockBackend) -> Session
{
    let mut session = session(mock);
    session.launch(&spec()).expect("launch");
    let _ = mock.take_calls();
    session
}

fn software(address: u64) -> BreakpointRequest
{
    BreakpointRequest::Software {
        address: Address::new(address),
    }
}

fn original(address: u64) -> Vec<u8>
{
    (address - CODE..address - CODE + 4).map(|b| b as u8).collect()
}

fn peek(mock: &MockBackend, address: u64) -> Vec<u8>
{
    mock.peek_memory(Address::new(address), 4).expect("mapped")
}

fn is_unsupported(result: &Result<BreakpointId>) -> bool
{
    matches!(result, Err(DebuggerError::Backend(BackendError::Unsupported(_))))
}

#[test]
fn session_is_send()
{
    fn assert_send<T: Send>() {}
    assert_send::<Session>();
}

#[test]
fn launch_selects_first_thread_and_has_no_last_stop()
{
    let mock = process();
    let mut session = session(&mock);
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.pid(), None);
    assert_eq!(session.architecture(), Architecture::Arm64);

    assert_eq!(session.launch(&spec()).expect("launch"), ProcessId(42));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.pid(), Some(ProcessId(42)));
    assert_eq!(session.selected_thread(), Some(T1));
    assert!(session.last_stop().is_none());
    assert_eq!(session.threads().expect("threads"), vec![T1, T2]);

    session.select_thread(T2).expect("select");
    assert_eq!(session.read_registers().expect("registers").pc, Address::new(CODE + 4));
    assert!(matches!(
        session.select_thread(ThreadId(9)),
        Err(DebuggerError::ThreadNotFound(9))
    ));
    assert_eq!(session.selected_thread(), Some(T2));
}

#[test]
fn attach_records_pid_and_stops()
{
    let mock = process();
    let mut session = session(&mock);
    session.attach(ProcessId(77)).expect("attach");
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.pid(), Some(ProcessId(77)));
    assert_eq!(session.selected_thread(), Some(T1));
    assert!(session.last_stop().is_none());
    assert_eq!(mock.calls()[0], MockCall::Attach(ProcessId(77)));
}

#[test]
fn breakpoint_insert_remove_disable_enable_restore_bytes()
{
    let mock = process();
    let mut session = launched(&mock);

    let id = session.add_breakpoint(software(BP)).expect("add");
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
    let info = session.breakpoint_info(id).expect("info");
    assert!(info.enabled);
    assert_eq!(info.state, BreakpointState::Resolved);
    assert_eq!(info.address, Address::new(BP));
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::ReadMemory {
                address: Address::new(BP),
                len: 4
            },
            MockCall::WriteMemory {
                address: Address::new(BP),
                data: TRAP.to_vec()
            },
        ]
    );

    session.disable_breakpoint(id).expect("disable");
    assert_eq!(peek(&mock, BP), original(BP));
    let info = session.breakpoint_info(id).expect("info");
    assert!(!info.enabled);
    assert_eq!(info.state, BreakpointState::Disabled);
    // Disabling twice does nothing.
    let _ = mock.take_calls();
    session.disable_breakpoint(id).expect("disable again");
    assert!(mock.calls().is_empty());

    session.enable_breakpoint(id).expect("enable");
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
    // Enabling writes the trap without reading memory again.
    assert_eq!(
        mock.take_calls(),
        [MockCall::WriteMemory {
            address: Address::new(BP),
            data: TRAP.to_vec()
        }]
    );
    assert!(session.breakpoint_info(id).expect("info").enabled);

    session.remove_breakpoint(id).expect("remove");
    assert_eq!(peek(&mock, BP), original(BP));
    assert!(session.breakpoints().is_empty());
    assert!(matches!(
        session.breakpoint_info(id),
        Err(DebuggerError::BreakpointIdNotFound(_))
    ));
    assert!(matches!(
        session.remove_breakpoint(id),
        Err(DebuggerError::BreakpointIdNotFound(_))
    ));

    // Removing a disabled breakpoint does not write memory.
    let id = session.add_breakpoint(software(BP)).expect("add");
    session.disable_breakpoint(id).expect("disable");
    let _ = mock.take_calls();
    session.remove_breakpoint(id).expect("remove disabled");
    assert!(mock.calls().is_empty());
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn detach_restores_every_trap()
{
    let mock = process();
    let mut session = launched(&mock);
    let first = session.add_breakpoint(software(BP)).expect("add");
    session.add_breakpoint(software(BP2)).expect("add");
    let disabled = session.add_breakpoint(software(CODE)).expect("add");
    session.disable_breakpoint(disabled).expect("disable");
    assert_eq!(peek(&mock, BP2), TRAP.to_vec());
    assert_eq!(session.breakpoints().len(), 3);
    assert_eq!(session.breakpoints()[0].id, first);

    session.detach().expect("detach");
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(peek(&mock, BP2), original(BP2));
    assert_eq!(peek(&mock, CODE), original(CODE));
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.pid(), None);
    assert_eq!(session.selected_thread(), None);
    assert!(session.breakpoints().is_empty());
    assert_eq!(mock.state(), MockState::NoProcess);
    assert_eq!(mock.calls().last(), Some(&MockCall::Detach));

    // A new process can be debugged afterwards.
    session.launch(&spec()).expect("relaunch");
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn detach_keeps_the_process_when_a_restore_fails()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    mock.fail_next(
        MockCallKind::WriteMemory,
        BackendError::Os {
            code: 1,
            message: "write".into(),
        },
    );
    assert!(matches!(
        session.detach(),
        Err(DebuggerError::Backend(BackendError::Os { code: 1, .. }))
    ));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(mock.state(), MockState::Stopped);
    assert!(!mock.calls().contains(&MockCall::Detach));
    assert!(session.breakpoint_info(id).expect("still there").enabled);
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
}

/// Every call that needs a process, with the error it must return.
fn assert_all_refused(session: &mut Session, expect: fn(&DebuggerError) -> bool, what: &str)
{
    let a = Address::new(CODE);
    let checks: Vec<(&str, Option<DebuggerError>)> = vec![
        ("detach", session.detach().err()),
        ("resume", session.resume().err()),
        ("step_instruction", session.step_instruction(T1).err()),
        ("threads", session.threads().err()),
        ("select_thread", session.select_thread(T1).err()),
        ("read_registers", session.read_registers().err()),
        ("read_registers_for", session.read_registers_for(T1).err()),
        ("write_registers", session.write_registers(&arm64_registers()).err()),
        (
            "write_registers_for",
            session.write_registers_for(T1, &arm64_registers()).err(),
        ),
        ("read_memory", session.read_memory(a, 4).err()),
        ("write_memory", session.write_memory(a, &[0]).err()),
        ("memory_regions", session.memory_regions().err()),
        ("add_breakpoint", session.add_breakpoint(software(BP)).err()),
        (
            "add_breakpoint(hardware)",
            session.add_breakpoint(BreakpointRequest::Hardware { address: a }).err(),
        ),
        (
            "remove_breakpoint",
            session.remove_breakpoint(BreakpointId::from_raw(1)).err(),
        ),
        (
            "enable_breakpoint",
            session.enable_breakpoint(BreakpointId::from_raw(1)).err(),
        ),
        (
            "disable_breakpoint",
            session.disable_breakpoint(BreakpointId::from_raw(1)).err(),
        ),
        ("stack_trace", session.stack_trace(8).err()),
        ("symbolicate", session.symbolicate(a).err()),
        ("images", session.images().err()),
    ];
    for (call, error) in checks {
        let error = error.unwrap_or_else(|| panic!("{call} succeeded {what}"));
        assert!(expect(&error), "{call} {what}: {error:?}");
    }
}

#[test]
fn calls_before_launch_are_not_attached_and_reach_no_backend()
{
    let mock = process();
    let mut session = session(&mock);
    assert_all_refused(&mut session, |e| matches!(e, DebuggerError::NotAttached), "before launch");
    assert!(matches!(session.wait(Some(Duration::ZERO)), Err(DebuggerError::NotAttached)));
    assert!(matches!(session.wait(None), Err(DebuggerError::NotAttached)));
    assert!(matches!(
        session.breakpoint_info(BreakpointId::from_raw(1)),
        Err(DebuggerError::NotAttached)
    ));
    assert!(session.breakpoints().is_empty());
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::NotStarted);
}

#[test]
fn calls_while_running_are_not_stopped_and_reach_no_backend()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    assert_eq!(session.state(), SessionState::Running);
    let _ = mock.take_calls();

    assert_all_refused(&mut session, |e| matches!(e, DebuggerError::NotStopped), "while running");
    assert!(matches!(session.launch(&spec()), Err(DebuggerError::AlreadyAttached)));
    assert!(matches!(session.attach(ProcessId(5)), Err(DebuggerError::AlreadyAttached)));
    // Reading the table is fine while running.
    assert!(session.breakpoint_info(id).expect("info").enabled);
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::Running);

    // Waiting is legal; the empty script reports nothing.
    assert_eq!(session.wait(Some(Duration::ZERO)).expect("wait"), None);
    assert_eq!(session.state(), SessionState::Running);
}

#[test]
fn launch_and_attach_while_stopped_are_already_attached()
{
    let mock = process();
    let mut session = launched(&mock);
    assert!(matches!(session.launch(&spec()), Err(DebuggerError::AlreadyAttached)));
    assert!(matches!(session.attach(ProcessId(5)), Err(DebuggerError::AlreadyAttached)));
    assert!(mock.calls().is_empty());
}

#[test]
fn calls_after_exit_are_not_attached_and_reach_no_backend()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    let mut session = launched(&mock);
    session.resume().expect("resume");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
    let _ = mock.take_calls();

    assert_all_refused(&mut session, |e| matches!(e, DebuggerError::NotAttached), "after exit");
    assert!(matches!(session.wait(None), Err(DebuggerError::NotAttached)));
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::Exited);
}

#[test]
fn wait_none_while_stopped_is_not_running_instead_of_blocking()
{
    let mock = process();
    let mut session = launched(&mock);
    assert!(matches!(session.wait(None), Err(DebuggerError::NotRunning)));
    // The backend was only polled.
    assert_eq!(mock.take_calls(), [MockCall::Wait(Some(Duration::ZERO))]);
    // A finite timeout is passed through.
    assert_eq!(session.wait(Some(Duration::from_millis(5))).expect("wait"), None);
    assert_eq!(mock.take_calls(), [MockCall::Wait(Some(Duration::from_millis(5)))]);
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn resume_to_breakpoint_stop_follows_thread_and_counts_hit()
{
    let mock = process();
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP),
        })
        .set_pc(T2, Address::new(BP))
        .expect_resume(ResumeRequest::continue_all()),
    );
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    assert_eq!(session.selected_thread(), Some(T1));

    session.resume().expect("resume");
    assert_eq!(session.state(), SessionState::Running);
    assert!(session.last_stop().is_none());

    let event = session.wait(None).expect("wait").expect("event");
    let expected = StopEvent::Breakpoint {
        thread: T2,
        pc: Address::new(BP),
    };
    assert_eq!(event, expected);
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.last_stop(), Some(&expected));
    assert_eq!(session.selected_thread(), Some(T2));
    assert_eq!(session.read_registers().expect("registers").pc, Address::new(BP));
    assert_eq!(session.breakpoint_info(id).expect("info").hit_count, 1);
}

#[test]
fn disabled_breakpoint_hit_is_not_counted()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Breakpoint {
        thread: T1,
        pc: Address::new(BP),
    }));
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    session.disable_breakpoint(id).expect("disable");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.breakpoint_info(id).expect("info").hit_count, 0);
}

#[test]
fn selection_falls_back_to_first_thread_when_selected_thread_exits()
{
    let mock = process();
    mock.script(
        MockStop::new(StopEvent::Interrupted { thread: None })
            .remove_thread(T2)
            .add_thread(ThreadId(3), regs_at(CODE)),
    );
    let mut session = launched(&mock);
    session.select_thread(T2).expect("select");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.selected_thread(), Some(T1));
    assert_eq!(session.threads().expect("threads"), vec![T1, ThreadId(3)]);
}

#[test]
fn selection_is_kept_when_the_event_names_no_thread()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Interrupted { thread: None }));
    let mut session = launched(&mock);
    session.select_thread(T2).expect("select");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.selected_thread(), Some(T2));
}

#[test]
fn terminal_event_exits_clears_process_and_allows_relaunch()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Killed { signal: 9 }));
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Killed { signal: 9 }));

    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.pid(), None);
    assert_eq!(session.selected_thread(), None);
    assert_eq!(session.last_stop(), Some(&StopEvent::Killed { signal: 9 }));
    // The traps went away with the process: the table is cleared.
    assert!(session.breakpoints().is_empty());

    assert_eq!(session.launch(&spec()).expect("relaunch"), ProcessId(42));
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(session.last_stop().is_none());
    assert_eq!(session.selected_thread(), Some(T1));
    // The new breakpoint reads the bytes now in memory (the old trap, since the mock
    // keeps its memory across processes) and gets a fresh entry.
    let id = session.add_breakpoint(software(BP2)).expect("add after relaunch");
    assert_eq!(session.breakpoints().len(), 1);
    assert_eq!(session.breakpoints()[0].id, id);
}

#[test]
fn resume_drains_pending_events_first()
{
    let mock = process();
    mock.script_simultaneous([
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        }),
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP2),
        }),
    ]);
    let mut session = launched(&mock);
    let first = session.add_breakpoint(software(BP)).expect("add");
    let second = session.add_breakpoint(software(BP2)).expect("add");
    session.resume().expect("resume");
    let event = session.wait(None).expect("wait").expect("event");
    assert_eq!(event.thread(), Some(T1));
    assert_eq!(mock.pending_len(), 1);
    let _ = mock.take_calls();

    // The second event is still queued: resume drains it instead of resuming.
    session.resume().expect("resume with an event pending");
    assert_eq!(session.state(), SessionState::Running);
    assert!(!mock.calls().iter().any(|call| matches!(call, MockCall::Resume(_))));
    let event = session.wait(None).expect("wait").expect("event");
    assert_eq!(event.thread(), Some(T2));
    assert_eq!(session.selected_thread(), Some(T2));
    assert_eq!(session.breakpoint_info(first).expect("info").hit_count, 1);
    assert_eq!(session.breakpoint_info(second).expect("info").hit_count, 1);

    // Nothing pending now: resume reaches the backend.
    let _ = mock.take_calls();
    session.resume().expect("resume");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::ReadRegisters(T1),
            MockCall::ReadRegisters(T2),
            MockCall::Resume(ResumeRequest::continue_all())
        ]
    );
}

#[test]
fn wait_while_stopped_returns_pending_event()
{
    let mock = process();
    mock.script_simultaneous([
        MockStop::new(StopEvent::Interrupted { thread: Some(T1) }),
        MockStop::new(StopEvent::Signal { thread: T2, signal: 30 }),
    ]);
    let mut session = launched(&mock);
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(
        session.wait(None).expect("pending"),
        Some(StopEvent::Signal { thread: T2, signal: 30 })
    );
    assert_eq!(session.selected_thread(), Some(T2));
    assert!(matches!(session.wait(None), Err(DebuggerError::NotRunning)));
}

#[test]
fn failed_trap_write_leaves_table_unchanged()
{
    let mock = process();
    let mut session = launched(&mock);
    mock.fail_next(
        MockCallKind::WriteMemory,
        BackendError::Os {
            code: 2,
            message: "protection".into(),
        },
    );
    let result = session.add_breakpoint(software(BP));
    assert!(matches!(
        result,
        Err(DebuggerError::Backend(BackendError::Os { code: 2, .. }))
    ));
    assert!(session.breakpoints().is_empty());
    assert_eq!(peek(&mock, BP), original(BP));

    // A failed read leaves it unchanged too.
    mock.fail_next(MockCallKind::ReadMemory, BackendError::NotFound("unmapped".into()));
    assert!(matches!(
        session.add_breakpoint(software(BP)),
        Err(DebuggerError::Backend(BackendError::NotFound(_)))
    ));
    assert!(session.breakpoints().is_empty());

    // A failed restore keeps the breakpoint enabled; a failed re-insert keeps it disabled.
    let id = session.add_breakpoint(software(BP)).expect("add");
    let os = || BackendError::Os {
        code: 3,
        message: "write".into(),
    };
    mock.fail_next(MockCallKind::WriteMemory, os());
    assert!(session.disable_breakpoint(id).is_err());
    assert!(session.breakpoint_info(id).expect("info").enabled);
    mock.fail_next(MockCallKind::WriteMemory, os());
    assert!(session.remove_breakpoint(id).is_err());
    assert!(session.breakpoint_info(id).is_ok());
    session.disable_breakpoint(id).expect("disable");
    mock.fail_next(MockCallKind::WriteMemory, os());
    assert!(session.enable_breakpoint(id).is_err());
    assert!(!session.breakpoint_info(id).expect("info").enabled);
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn permission_denied_from_launch_surfaces_as_permission_denied()
{
    let mock = process();
    mock.fail_next(MockCallKind::Launch, BackendError::PermissionDenied("task_for_pid".into()));
    let mut session = session(&mock);
    let Err(DebuggerError::PermissionDenied(message)) = session.launch(&spec()) else {
        panic!("expected PermissionDenied")
    };
    assert_eq!(message, "task_for_pid");
    assert_eq!(session.state(), SessionState::NotStarted);
    assert_eq!(session.pid(), None);
    session.launch(&spec()).expect("second launch");
}

#[test]
fn backend_error_conversion()
{
    assert!(matches!(
        DebuggerError::from(BackendError::PermissionDenied("x".into())),
        DebuggerError::PermissionDenied(m) if m == "x"
    ));
    let error = DebuggerError::from(BackendError::NotFound("thread 3".into()));
    assert_eq!(error.to_string(), "not found: thread 3");
    assert!(matches!(error, DebuggerError::Backend(BackendError::NotFound(_))));
}

#[test]
fn duplicate_software_breakpoint_returns_same_id()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();
    assert_eq!(session.add_breakpoint(software(BP)).expect("again"), id);
    assert!(mock.calls().is_empty());
    assert_eq!(session.breakpoints().len(), 1);
    // The saved original bytes are still the real instruction, not the trap.
    session.remove_breakpoint(id).expect("remove");
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn hardware_breakpoints_and_watchpoints_are_unsupported()
{
    let mock = process();
    let mut session = launched(&mock);
    let address = Address::new(BP);
    assert!(is_unsupported(
        &session.add_breakpoint(BreakpointRequest::Hardware { address })
    ));
    assert!(is_unsupported(&session.add_breakpoint(BreakpointRequest::Watchpoint {
        address,
        length: 8,
        access: WatchpointAccess::Write,
    })));
    assert!(mock.calls().is_empty());
    assert!(session.breakpoints().is_empty());
}

#[test]
fn unsupported_architecture_refuses_software_breakpoints()
{
    let mock = process().with_architecture(Architecture::X86_64);
    let mut session = launched(&mock);
    assert!(is_unsupported(&session.add_breakpoint(software(BP))));
    assert!(mock.calls().is_empty());
}

#[test]
fn stack_trace_without_images_returns_the_current_frame()
{
    let mock = process();
    let mut session = launched(&mock);
    let frames = session.stack_trace(16).expect("stack trace");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].pc, Address::new(CODE));
    assert_eq!(frames[0].thread, T1);
    assert!(frames[0].symbol.is_none());
    assert_eq!(mock.take_calls(), [MockCall::ReadRegisters(T1), MockCall::Images]);

    assert!(session.stack_trace(0).expect("zero frames").is_empty());
    assert_eq!(
        session
            .symbolicate(Address::new(CODE))
            .expect("symbolicate")
            .map(|s| s.frames.len()),
        None
    );
}

#[test]
fn stack_trace_reads_memory_through_the_backend_and_skips_bad_images()
{
    // Thread 1 with a frame pointer at 0x2000: saved fp 0, saved lr 0x1010.
    let mut registers = regs_at(CODE);
    registers.fp = Address::new(0x2000);
    registers.sp = Address::new(0x2000);
    let mut stack = vec![0u8; 16];
    stack[8..].copy_from_slice(&0x1010u64.to_le_bytes());
    let mock = MockBackend::new()
        .with_thread(T1, registers)
        .with_region(Address::new(CODE), "r-x", vec![0; 32])
        .with_region(Address::new(0x2000), "rw-", stack)
        .with_image(LoadedImage {
            path: "/nonexistent/ferros-session-test-image".into(),
            load_address: Address::new(CODE),
        });
    let mut session = launched(&mock);
    let frames = session.stack_trace(8).expect("stack trace");
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(pcs[..2], [CODE, 0x1010], "{frames:?}");
    assert!(mock.calls().contains(&MockCall::ReadMemory {
        address: Address::new(0x2000),
        len: 8
    }));

    // Symbolication still works after an image failed to load: the backend is asked for
    // its image list and nothing else.
    let _ = mock.take_calls();
    session.symbolicate(Address::new(CODE)).expect("symbolicate");
    assert_eq!(mock.take_calls(), [MockCall::Images]);
}

#[test]
fn stack_trace_ignores_unsupported_image_listing()
{
    let mock = process();
    mock.fail_next(MockCallKind::Images, BackendError::Unsupported("no images".into()));
    let mut session = launched(&mock);
    assert_eq!(session.stack_trace(4).expect("stack trace").len(), 1);

    mock.fail_next(
        MockCallKind::Images,
        BackendError::Os {
            code: 4,
            message: "dyld".into(),
        },
    );
    assert!(matches!(
        session.stack_trace(4),
        Err(DebuggerError::Backend(BackendError::Os { code: 4, .. }))
    ));
}

#[test]
fn register_and_memory_pass_through()
{
    let mock = process();
    let mut session = launched(&mock);
    let mut registers = session.read_registers().expect("read");
    registers.general[0] = 7;
    session.write_registers(&registers).expect("write");
    assert_eq!(mock.peek_registers(T1).expect("t1").general[0], 7);
    session.write_registers_for(T2, &registers).expect("write t2");
    assert_eq!(session.read_registers_for(T2).expect("read t2").general[0], 7);
    assert!(matches!(
        session.read_registers_for(ThreadId(9)),
        Err(DebuggerError::Backend(BackendError::NotFound(_)))
    ));

    session.write_memory(Address::new(CODE), &[9, 9]).expect("write");
    assert_eq!(session.read_memory(Address::new(CODE), 3).expect("read"), vec![9, 9, 2]);
    assert_eq!(session.memory_regions().expect("regions").len(), 1);
    assert!(session.images().expect("images").is_empty());
}

#[test]
fn process_without_threads_has_no_selection()
{
    let mock = MockBackend::new().with_region(Address::new(CODE), "r-x", vec![0; 8]);
    let mut session = launched(&mock);
    assert_eq!(session.selected_thread(), None);
    assert!(matches!(session.read_registers(), Err(DebuggerError::NoThreadSelected)));
    assert!(matches!(session.stack_trace(4), Err(DebuggerError::NoThreadSelected)));
}

#[test]
fn backend_mut_bypasses_the_table()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    session
        .backend_mut()
        .write_memory(Address::new(BP), &original(BP))
        .expect("raw write");
    // The table still believes the trap is inserted.
    assert!(session.breakpoint_info(id).expect("info").enabled);
    assert_eq!(session.backend().architecture(), Architecture::Arm64);
}

/// Continuing from a breakpoint stop must first step the stopped thread
/// over the trap with the original instruction restored.
#[test]
fn resume_from_breakpoint_steps_over_the_trap()
{
    let mock = process();
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        })
        .set_pc(T1, Address::new(BP))
        .expect_resume(ResumeRequest::continue_all()),
    );
    mock.script(
        MockStop::new(StopEvent::SingleStep { thread: T1 })
            .set_pc(T1, Address::new(BP + 4))
            .expect_resume(ResumeRequest::all(ThreadAction::Suspend).with_thread(T1, ThreadAction::Step)),
    );
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    session.wait(None).expect("breakpoint stop");
    session.resume().expect("resume from the breakpoint");
    assert_eq!(session.wait(None).expect("wait"), Some(StopEvent::Exited { code: 0 }));
}

/// Reads through the session show the original bytes, not traps.
#[test]
fn read_memory_hides_trap_bytes()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    assert_eq!(session.read_memory(Address::new(BP), 4).expect("read"), original(BP));
}

#[test]
fn dropping_a_stopped_session_restores_every_trap()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let disabled = session.add_breakpoint(software(BP2)).expect("add second");
    session.disable_breakpoint(disabled).expect("disable");
    assert_eq!(mock.peek_memory(Address::new(BP), 4).expect("peek"), TRAP);

    drop(session);
    assert_eq!(mock.peek_memory(Address::new(BP), 4).expect("peek"), original(BP));
    assert_eq!(mock.peek_memory(Address::new(BP2), 4).expect("peek"), original(BP2));
}

#[test]
fn overlapping_breakpoint_is_refused_before_any_backend_call()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();

    for address in [BP + 1, BP + 3, BP - 1, BP - 3] {
        assert!(
            matches!(
                session.add_breakpoint(software(address)),
                Err(DebuggerError::InvalidArgument(_))
            ),
            "a trap at 0x{address:x} overlaps the one at 0x{BP:x}"
        );
    }
    assert!(mock.take_calls().is_empty(), "an overlapping breakpoint reached the backend");
    assert_eq!(session.breakpoints().len(), 1);

    // The neighbouring instruction does not overlap.
    session.add_breakpoint(software(BP + 4)).expect("add at the next instruction");
    drop(session);
    assert_eq!(
        mock.peek_memory(Address::new(BP), 8).expect("peek"),
        [original(BP), original(BP + 4)].concat()
    );
}

#[test]
fn failed_thread_listing_after_a_stop_does_not_keep_a_dead_thread_selected()
{
    let mock = process();
    let mut session = launched(&mock);
    session.select_thread(T2).expect("select T2");
    mock.script(MockStop::new(StopEvent::Interrupted { thread: None }).remove_thread(T2));
    session.resume().expect("resume");
    mock.fail_next(
        MockCallKind::Threads,
        BackendError::Os {
            code: 5,
            message: "threads".into(),
        },
    );
    session.wait(Some(Duration::ZERO)).expect("wait").expect("an event");
    assert_eq!(session.selected_thread(), None, "T2 exited but is still selected");

    assert_eq!(session.threads().expect("threads"), vec![T1]);
    assert_eq!(session.selected_thread(), Some(T1));
}

// --- Lifecycle: interrupt, kill, signal pass-through and ownership on drop (T06, #32) ---

fn attached(mock: &MockBackend) -> Session
{
    let mut session = session(mock);
    session.attach(ProcessId(77)).expect("attach");
    let _ = mock.take_calls();
    session
}

fn kinds(calls: &[MockCall]) -> Vec<MockCallKind>
{
    calls.iter().map(MockCall::kind).collect()
}

#[test]
fn ownership_follows_launch_attach_detach_and_kill()
{
    let mock = process();
    let mut session = session(&mock);
    assert_eq!(session.ownership(), None);
    session.launch(&spec()).expect("launch");
    assert_eq!(session.ownership(), Some(ProcessOwnership::Launched));
    session.kill().expect("kill");
    assert_eq!(session.ownership(), None);
    session.attach(ProcessId(77)).expect("attach");
    assert_eq!(session.ownership(), Some(ProcessOwnership::Attached));
    session.detach().expect("detach");
    assert_eq!(session.ownership(), None);

    // A terminal event ends ownership too, for a launched and an attached process.
    for attach in [false, true] {
        mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
        if attach {
            session.attach(ProcessId(77)).expect("attach");
        } else {
            session.launch(&spec()).expect("launch");
        }
        session.resume().expect("resume");
        session.wait(None).expect("wait");
        assert_eq!(session.state(), SessionState::Exited);
        assert_eq!(session.ownership(), None, "attach = {attach}");
    }
}

#[test]
fn interrupt_while_running_reports_interrupted_and_stops()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    let _ = mock.take_calls();

    session.interrupt().expect("interrupt");
    assert_eq!(mock.take_calls(), [MockCall::Interrupt]);
    assert_eq!(session.state(), SessionState::Running, "the stop is reported by wait");
    assert_eq!(
        session.wait(Some(Duration::from_secs(1))).expect("wait"),
        Some(StopEvent::Interrupted { thread: None })
    );
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.last_stop(), Some(&StopEvent::Interrupted { thread: None }));
}

#[test]
fn interrupt_while_stopped_calls_no_backend()
{
    let mock = process();
    let mut session = launched(&mock);
    session.interrupt().expect("interrupt while stopped");
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn interrupt_failure_leaves_the_session_running()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    mock.fail_next(
        MockCallKind::Interrupt,
        BackendError::Os {
            code: 5,
            message: "no".into(),
        },
    );
    assert!(matches!(
        session.interrupt(),
        Err(DebuggerError::Backend(BackendError::Os { code: 5, .. }))
    ));
    assert_eq!(session.state(), SessionState::Running);
}

#[test]
fn interrupt_and_kill_without_a_process_are_not_attached()
{
    let mock = process();
    let mut session = session(&mock);
    assert!(matches!(session.interrupt(), Err(DebuggerError::NotAttached)));
    assert!(matches!(session.kill(), Err(DebuggerError::NotAttached)));
    assert!(mock.calls().is_empty());

    // After exit as well.
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    session.launch(&spec()).expect("launch");
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    assert_eq!(session.state(), SessionState::Exited);
    let _ = mock.take_calls();
    assert!(matches!(session.interrupt(), Err(DebuggerError::NotAttached)));
    assert!(matches!(session.kill(), Err(DebuggerError::NotAttached)));
    assert!(mock.calls().is_empty());
    // The exit event is kept as the last stop.
    assert_eq!(session.last_stop(), Some(&StopEvent::Exited { code: 0 }));
}

fn assert_killed(session: &Session)
{
    assert_eq!(session.state(), SessionState::Exited);
    assert_eq!(session.last_stop(), Some(&StopEvent::Killed { signal: SIGKILL }));
    assert_eq!(session.pid(), None);
    assert_eq!(session.selected_thread(), None);
    assert!(session.breakpoints().is_empty());
}

#[test]
fn kill_while_stopped_exits_and_allows_relaunch()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();

    assert_eq!(session.kill().expect("kill"), StopEvent::Killed { signal: 9 });
    // No trap is restored: the memory that held it is gone.
    assert_eq!(mock.take_calls(), [MockCall::Kill]);
    assert_killed(&session);
    assert_eq!(mock.state(), MockState::NoProcess);

    assert_eq!(session.launch(&spec()).expect("relaunch"), ProcessId(42));
    assert_eq!(session.state(), SessionState::Stopped);
    assert!(session.last_stop().is_none());
}

#[test]
fn kill_while_running_exits()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    let _ = mock.take_calls();

    session.kill().expect("kill while running");
    assert_eq!(mock.take_calls(), [MockCall::Kill]);
    assert_killed(&session);
}

#[test]
fn kill_failure_leaves_state_and_breakpoints_unchanged()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    mock.fail_next(MockCallKind::Kill, BackendError::PermissionDenied("no".into()));
    assert!(matches!(session.kill(), Err(DebuggerError::PermissionDenied(_))));
    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.pid(), Some(ProcessId(42)));
    assert!(session.breakpoint_info(id).expect("info").enabled);
    assert_eq!(peek(&mock, BP), TRAP);
}

#[test]
fn resume_with_signal_passes_a_held_signal_and_resume_discards_it()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Signal { thread: T2, signal: 11 }));
    mock.script(MockStop::new(StopEvent::Signal { thread: T2, signal: 11 }));
    mock.script(MockStop::new(StopEvent::Interrupted { thread: Some(T1) }));
    let mut session = launched(&mock);

    // First stop: pass the signal on.
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    let _ = mock.take_calls();
    session.resume_with_signal().expect("resume with signal");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::Resume(ResumeRequest::continue_all().with_signal(T2, 11))
        ]
    );

    // Second signal stop: plain resume discards it.
    session.wait(None).expect("wait");
    let _ = mock.take_calls();
    session.resume().expect("resume");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::Resume(ResumeRequest::continue_all())
        ]
    );

    // After a non-signal stop, resume_with_signal passes nothing.
    session.wait(None).expect("wait");
    let _ = mock.take_calls();
    session.resume_with_signal().expect("resume with signal");
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Wait(Some(Duration::ZERO)),
            MockCall::Resume(ResumeRequest::continue_all())
        ]
    );
}

#[test]
fn resume_with_signal_while_running_is_not_stopped()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    let _ = mock.take_calls();
    assert!(matches!(session.resume_with_signal(), Err(DebuggerError::NotStopped)));
    assert!(mock.calls().is_empty());
}

#[test]
fn dropping_a_launched_session_kills_the_process()
{
    // Stopped.
    let mock = process();
    let session = launched(&mock);
    drop(session);
    assert_eq!(kinds(&mock.take_calls()), [MockCallKind::Kill]);
    assert_eq!(mock.state(), MockState::NoProcess);

    // Running.
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    let _ = mock.take_calls();
    drop(session);
    assert_eq!(mock.take_calls(), [MockCall::Kill]);
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn dropping_an_attached_stopped_session_restores_traps_then_detaches()
{
    let mock = process();
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();

    drop(session);
    let calls = mock.take_calls();
    assert_eq!(kinds(&calls), [MockCallKind::WriteMemory, MockCallKind::Detach]);
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn dropping_an_attached_running_session_interrupts_restores_and_detaches()
{
    let mock = process();
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    let _ = mock.take_calls();

    drop(session);
    let calls = mock.take_calls();
    let kinds = kinds(&calls);
    assert_eq!(kinds.first(), Some(&MockCallKind::Interrupt), "{calls:?}");
    let write = kinds
        .iter()
        .position(|k| *k == MockCallKind::WriteMemory)
        .expect("trap restored");
    let wait = kinds.iter().position(|k| *k == MockCallKind::Wait).expect("stop awaited");
    assert!(wait < write, "{calls:?}");
    assert_eq!(kinds.last(), Some(&MockCallKind::Detach), "{calls:?}");
    assert!(!kinds.contains(&MockCallKind::Kill));
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn dropping_an_attached_running_session_drains_simultaneous_events_before_detaching()
{
    let mock = process();
    mock.script_simultaneous([
        MockStop::new(StopEvent::Breakpoint {
            thread: T1,
            pc: Address::new(BP),
        }),
        MockStop::new(StopEvent::Breakpoint {
            thread: T2,
            pc: Address::new(BP),
        }),
    ]);
    let mut session = attached(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    session.resume().expect("resume");
    // The interrupt raced the scripted stops; either way every event is drained.
    drop(session);
    assert_eq!(mock.pending_len(), 0);
    assert_eq!(mock.state(), MockState::NoProcess);
    assert_eq!(peek(&mock, BP), original(BP));
    assert_eq!(mock.calls().last().map(MockCall::kind), Some(MockCallKind::Detach));
}

#[test]
fn dropping_an_attached_session_whose_process_exits_does_not_detach()
{
    let mock = process();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    let mut session = attached(&mock);
    session.resume().expect("resume");
    // The interrupt fails and the process exits instead: the drop sees it gone.
    mock.fail_next(
        MockCallKind::Interrupt,
        BackendError::Os {
            code: 1,
            message: "gone".into(),
        },
    );
    let _ = mock.take_calls();
    drop(session);
    let calls = mock.take_calls();
    assert!(!kinds(&calls).contains(&MockCallKind::Detach), "{calls:?}");
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn wait_with_timeout_while_running_and_nothing_happens_stays_running()
{
    let mock = process();
    let mut session = launched(&mock);
    session.resume().expect("resume");
    assert_eq!(session.wait(Some(Duration::from_millis(200))).expect("wait"), None);
    assert_eq!(session.state(), SessionState::Running);
}

#[path = "tests/stepping.rs"]
mod stepping;
