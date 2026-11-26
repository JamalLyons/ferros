//! Shared constants and helpers for the single-step tests.

use std::process::Command;
use std::time::Duration;

use ferros_core::{Address, Backend, BackendError, ResumeRequest, StopEvent, ThreadAction, ThreadId};
use ferros_macos::diagnostics;
use ferros_tests::Fixture;
use ferros_tests::arm64::{Flow, TRAP, flow};
use ferros_tests::harness::Debuggee;
use ferros_tests::symbols::function_address;

/// How long a test waits for a stop.
pub(crate) const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Iterations passed to `loop_counter`, so it outlives every test (10 ms each).
pub(crate) const LONG_LOOP: &str = "100000";

/// `SIGUSR1` on macOS.
pub(crate) const SIGUSR1: i32 = 30;

/// Instructions stepped in the stepping tests.
pub(crate) const STEPS: usize = 8;

/// The session's backend.
pub(crate) fn backend(d: &mut Debuggee) -> &mut dyn Backend
{
    d.session_mut().backend_mut()
}

/// The pc of `thread`.
pub(crate) fn pc_of(d: &mut Debuggee, thread: ThreadId) -> Address
{
    backend(d)
        .read_registers(thread)
        .unwrap_or_else(|err| panic!("read the registers of thread {thread:?}: {err}"))
        .pc
}

/// The 32-bit instruction at `address`.
pub(crate) fn instruction_at(d: &mut Debuggee, address: Address) -> u32
{
    let bytes = backend(d)
        .read_memory(address, 4)
        .unwrap_or_else(|err| panic!("read the instruction at {address}: {err}"));
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

/// Write a trap at `address` and return the bytes it replaced.
pub(crate) fn plant_trap(d: &mut Debuggee, address: Address) -> Vec<u8>
{
    let original = backend(d).read_memory(address, TRAP.len()).expect("read the original bytes");
    backend(d).write_memory(address, &TRAP).expect("write the trap");
    original
}

/// Continue every thread and wait for the next stop, which must be a breakpoint stop at
/// `address`; returns its thread. Other threads' stops at the same trap, reported with no
/// resume in between, are drained and returned too.
pub(crate) fn run_to_trap(d: &mut Debuggee, address: Address) -> (ThreadId, Vec<ThreadId>)
{
    backend(d).resume(&ResumeRequest::continue_all()).expect("continue");
    let first = match backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for the trap") {
        Some(StopEvent::Breakpoint { thread, pc }) if pc == address => thread,
        other => panic!("expected a breakpoint stop at {address}, got {other:?}"),
    };
    let mut others = Vec::new();
    while let Some(event) = backend(d).wait(Some(Duration::ZERO)).expect("drain the stop") {
        match event {
            StopEvent::Breakpoint { thread, pc } if pc == address => others.push(thread),
            other => panic!("expected only breakpoint stops at {address}, got {other:?}"),
        }
    }
    (first, others)
}

/// Run the `threads` fixture to a trap at `worker_body`, which both of its workers call,
/// and return the worker that stopped first and the other one, both stopped at the trap.
///
/// `run_to_trap` drains only the peer stops published by then. The other worker's hit
/// can still be in flight and surface at a later `wait`, in place of the stop a test
/// asserts (#253). So, with the trap still armed and the first worker suspended, resume
/// the rest until that hit arrives. The trap stays planted; the caller restores it.
pub(crate) fn run_to_both_workers(d: &mut Debuggee, worker_body: Address) -> (ThreadId, ThreadId)
{
    let (first, other_hits) = run_to_trap(d, worker_body);
    let second = match other_hits.as_slice() {
        [thread] => *thread,
        [] => {
            let continue_others = ResumeRequest::continue_all().with_thread(first, ThreadAction::Suspend);
            match backend(d).resume(&continue_others) {
                Ok(()) | Err(BackendError::StopPending) => {}
                Err(err) => panic!("continue to the second worker: {err}"),
            }
            match backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for the second worker") {
                Some(StopEvent::Breakpoint { thread, pc }) if pc == worker_body => thread,
                other => panic!("expected the second worker's breakpoint, got {other:?}"),
            }
        }
        _ => panic!("expected exactly two workers, got additional hits {other_hits:?}"),
    };
    assert_ne!(first, second, "expected both workers to hit the trap");
    (first, second)
}

/// Resume with `thread` stepping and every other thread suspended, and wait for the stop.
pub(crate) fn step(d: &mut Debuggee, thread: ThreadId) -> StopEvent
{
    backend(d).resume(&ResumeRequest::step(thread)).expect("resume with a step");
    backend(d)
        .wait(Some(STOP_TIMEOUT))
        .expect("wait for the step")
        .expect("a stop after the step")
}

/// Step `thread` [`STEPS`] times from where it is: every stop must be its `SingleStep`,
/// and each new pc must follow from the instruction just executed. Returns how many
/// steps were straight-line (pc + 4).
pub(crate) fn step_and_check(d: &mut Debuggee, thread: ThreadId, mut each: impl FnMut(&mut Debuggee, usize)) -> usize
{
    let mut straight = 0;
    for n in 1..=STEPS {
        let pc = pc_of(d, thread).value();
        let insn = instruction_at(d, Address::from(pc));
        let event = step(d, thread);
        assert_eq!(
            event,
            StopEvent::SingleStep { thread },
            "step {n} from {pc:#x} ({insn:#010x}) ended with another stop"
        );
        assert_eq!(
            backend(d).wait(Some(Duration::ZERO)).expect("wait(0) after a step"),
            None,
            "step {n} reported more than one stop"
        );
        let next = pc_of(d, thread).value();
        match flow(pc, insn) {
            Flow::Next => {
                assert_eq!(next, pc + 4, "step {n}: {insn:#010x} at {pc:#x} is no branch");
                straight += 1;
            }
            Flow::Direct(target) => assert_eq!(next, target, "step {n}: branch {insn:#010x} at {pc:#x}"),
            Flow::Conditional(target) => assert!(
                next == target || next == pc + 4,
                "step {n}: conditional branch {insn:#010x} at {pc:#x} went elsewhere"
            ),
            Flow::Indirect => assert_ne!(next, pc, "step {n}: {insn:#010x} at {pc:#x} did not move"),
        }
        each(d, n);
    }
    straight
}

/// The `ps` state of `pid` (for example `S`, `R`, `T` or `Z`), empty if it does not exist.
pub(crate) fn process_state(pid: u32) -> String
{
    let output = Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
/// Number of threads of process `pid`, as `ps -M` lists them (one line per thread after
/// the header).
pub(crate) fn thread_count(pid: u32) -> usize
{
    let output = Command::new("ps")
        .args(["-M", "-p", &pid.to_string()])
        .output()
        .expect("run ps -M");
    String::from_utf8_lossy(&output.stdout).lines().count().saturating_sub(1)
}

/// How long a new thread is given to do work it must not do.
pub(crate) const BIRTH_QUIET: Duration = Duration::from_millis(400);

/// `brk #0` as a decoded instruction.
pub(crate) const BRK: u32 = u32::from_le_bytes(TRAP);

/// `stp xzr, xzr, [sp, #-16]!`, the first instruction of both pthread entries.
pub(crate) const STP_XZR_XZR_PREDEC: u32 = 0xa9bf_7fff;

/// The 32-bit instruction at `address` as it is in memory, including traps the backend
/// hides from `read_memory`.
pub(crate) fn raw_instruction_at(d: &Debuggee, address: Address) -> u32
{
    let bytes = diagnostics::raw_memory(d.pid(), address, 4)
        .unwrap_or_else(|err| panic!("read the raw instruction at {address}: {err}"));
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

/// `_thread_start` of `libsystem_pthread`, found from the outermost frame record of
/// `thread`, a running `std::thread`. `_thread_start` calls into pthread with the `bl`
/// that is its second instruction, so that record's return address is
/// `_thread_start + 8`.
pub(crate) fn thread_start_of(d: &mut Debuggee, thread: ThreadId) -> Address
{
    let mut fp = backend(d).read_registers(thread).expect("registers").fp.value();
    let mut outermost = None;
    for _ in 0..512 {
        if fp == 0 {
            break;
        }
        let record = backend(d).read_memory(Address::from(fp), 16).expect("read a frame record");
        outermost = Some(u64::from_le_bytes(record[8..16].try_into().expect("lr")));
        fp = u64::from_le_bytes(record[..8].try_into().expect("fp"));
    }
    let start = Address::from(outermost.expect("the thread has no frame records") - 8);
    assert_eq!(
        instruction_at(d, start),
        STP_XZR_XZR_PREDEC,
        "the outermost frame of {thread:?} does not return into _thread_start"
    );
    start
}

/// Stop at `spawn_later`, with every other thread's pc recorded.
pub(crate) fn stop_before_second_thread(d: &mut Debuggee, fixture: &Fixture) -> (ThreadId, Vec<(ThreadId, Address)>)
{
    let address = function_address(d, fixture, "spawn_later");
    let original = plant_trap(d, address);
    // Only the main thread calls spawn_later, once, so no peer can reach this trap.
    let (creator, _) = run_to_trap(d, address);
    backend(d).write_memory(address, &original).expect("restore");
    let images = backend(d).images().expect("images");
    assert!(
        images
            .iter()
            .any(|image| image.path.file_name().is_some_and(|name| name == "libsystem_pthread.dylib")),
        "dyld did not publish libsystem_pthread after the process ran: {images:?}"
    );
    let existing = backend(d).threads().expect("threads");
    assert!(existing.len() >= 2, "expected an existing worker: {existing:?}");
    let suspended = existing
        .into_iter()
        .filter(|&id| id != creator)
        .map(|id| (id, pc_of(d, id)))
        .collect();
    (creator, suspended)
}

/// Resume `creator` under default `Suspend` and require the new pthread to stay at a trap.
pub(crate) fn hold_new_thread(d: &mut Debuggee, fixture: &Fixture) -> (ThreadId, Vec<(ThreadId, Address)>, ThreadId)
{
    let (creator, suspended) = stop_before_second_thread(d, fixture);
    backend(d)
        .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
        .expect("resume creator");
    assert!(
        !d.wait_for_stdout("new worker ran", BIRTH_QUIET),
        "new thread executed application code during Suspend, stdout: {}",
        d.stdout()
    );
    assert_eq!(
        backend(d).wait(Some(Duration::from_millis(50))).expect("wait"),
        None,
        "the birth trap was delivered to the client"
    );
    backend(d).interrupt().expect("interrupt");
    assert!(matches!(
        backend(d).wait(Some(STOP_TIMEOUT)).expect("stop"),
        Some(StopEvent::Interrupted { .. })
    ));
    assert_suspended_still(d, &suspended);
    let known: Vec<_> = suspended.iter().map(|(id, _)| *id).chain(std::iter::once(creator)).collect();
    let newborn = backend(d)
        .threads()
        .expect("threads")
        .into_iter()
        .find(|id| !known.contains(id))
        .expect("the new thread was not enumerated");
    let pc = pc_of(d, newborn);
    assert_eq!(raw_instruction_at(d, pc), BRK, "new thread is not held on a trap at {pc}");
    (creator, suspended, newborn)
}

pub(crate) fn assert_suspended_still(d: &mut Debuggee, suspended: &[(ThreadId, Address)])
{
    for (id, pc) in suspended {
        assert_eq!(pc_of(d, *id), *pc, "existing suspended thread {id:?} moved");
    }
}
/// Stop with the creator at `spawn_second`, the first newborn held, and earlier threads recorded.
pub(crate) fn stop_before_second_spawn(d: &mut Debuggee, fixture: &Fixture)
-> (ThreadId, Vec<(ThreadId, Address)>, ThreadId)
{
    let address = function_address(d, fixture, "spawn_later");
    let second = function_address(d, fixture, "spawn_second");
    let original = plant_trap(d, address);
    // Only the main thread calls spawn_later, once, so no peer can reach this trap.
    let (creator, _) = run_to_trap(d, address);
    backend(d).write_memory(address, &original).expect("restore spawn_later");
    let second_original = plant_trap(d, second);
    let existing = backend(d).threads().expect("threads");
    let suspended: Vec<_> = existing
        .into_iter()
        .filter(|&id| id != creator)
        .map(|id| (id, pc_of(d, id)))
        .collect();
    backend(d)
        .resume(&ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue))
        .expect("resume creator");
    let hit = backend(d)
        .wait(Some(STOP_TIMEOUT))
        .expect("wait for spawn_second")
        .expect("spawn_second stop");
    assert_eq!(
        hit,
        StopEvent::Breakpoint {
            thread: creator,
            pc: second
        },
        "expected the creator at spawn_second"
    );
    assert_eq!(
        backend(d).wait(Some(Duration::ZERO)).expect("drain"),
        None,
        "the birth trap was delivered with spawn_second"
    );
    backend(d)
        .write_memory(second, &second_original)
        .expect("restore spawn_second");
    assert!(
        !d.stdout().contains("new worker ran") && !d.stdout().contains("second worker ran"),
        "a worker ran before step-off: {}",
        d.stdout()
    );
    let known: Vec<_> = suspended.iter().map(|(id, _)| *id).chain(std::iter::once(creator)).collect();
    let newborn = backend(d)
        .threads()
        .expect("threads")
        .into_iter()
        .find(|id| !known.contains(id))
        .expect("the first new thread was not enumerated");
    let newborn_pc = pc_of(d, newborn);
    assert_eq!(raw_instruction_at(d, newborn_pc), BRK);
    (creator, suspended, newborn)
}
