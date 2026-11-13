//! Software breakpoints through `ferros_core::Session` on the macOS backend, against real
//! fixture processes (issue #33).
//!
//! The engine plants trap bytes with `Backend::write_memory`, maps the raw pc of a trap
//! stop, and steps a thread over the breakpoint it sits on (original instruction back,
//! one step with the other threads suspended, trap re-armed) before every resume.
//! `Session::read_memory` hides the traps; these tests read the target's *raw* bytes
//! through the backend (`session.backend_mut().read_memory`) to see what is really there.
//!
//! Acceptance criteria of issue #33 (AC1-AC8) are named in each test's doc comment.
#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::Duration;

use ferros_core::{
    Address, BreakpointId, BreakpointRequest, DebuggerError, Session, SessionState, StopEvent, ThreadId, arch,
};
use ferros_macos::MacOSBackend;
use ferros_tests::arm64::{Flow, NOP, TRAP, flow, sp_decrement};
use ferros_tests::harness::{self, Spawned};
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture};

/// How long a test waits for one stop after resuming.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a test lets the target run when it expects *no* stop.
const QUIET_PERIOD: Duration = Duration::from_millis(300);

/// Iterations passed to `loop_counter` when it must outlive the test (10 ms each).
const LONG_LOOP: &str = "100000";

/// The raw bytes at `address` in the target, traps included.
fn raw(session: &mut Session, address: Address) -> Vec<u8>
{
    session
        .backend_mut()
        .read_memory(address, 4)
        .unwrap_or_else(|err| panic!("raw read at {address}: {err}"))
}

/// The instruction at `address` as the session shows it (traps hidden).
fn instruction_at(session: &mut Session, address: Address) -> u32
{
    let bytes = session
        .read_memory(address, 4)
        .unwrap_or_else(|err| panic!("read the instruction at {address}: {err}"));
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

/// Add a software breakpoint at `address`.
fn add(session: &mut Session, address: Address) -> BreakpointId
{
    session
        .add_breakpoint(BreakpointRequest::Software { address })
        .unwrap_or_else(|err| panic!("add_breakpoint at {address}: {err}"))
}

/// The next stop within [`STOP_TIMEOUT`]; panics on a timeout.
fn next_stop(session: &mut Session) -> StopEvent
{
    session
        .wait(Some(STOP_TIMEOUT))
        .expect("wait")
        .unwrap_or_else(|| panic!("no stop within {STOP_TIMEOUT:?}"))
}

/// Resume and wait for a breakpoint stop at `address`; returns its thread.
fn run_to(session: &mut Session, address: Address) -> ThreadId
{
    session.resume().expect("resume");
    match next_stop(session) {
        StopEvent::Breakpoint { thread, pc } if pc == address => thread,
        other => panic!("expected a breakpoint stop at {address}, got {other:?}"),
    }
}

/// Register `x0` of `thread` (the first argument at a function's entry).
fn x0(session: &mut Session, thread: ThreadId) -> u64
{
    session.read_registers_for(thread).expect("read_registers_for").general[0]
}

/// The pc of `thread`.
fn pc_of(session: &mut Session, thread: ThreadId) -> Address
{
    session.read_registers_for(thread).expect("read_registers_for").pc
}

/// The hit count of breakpoint `id`.
fn hits(session: &Session, id: BreakpointId) -> u64
{
    session.breakpoint_info(id).expect("breakpoint_info").hit_count
}

/// Resume and expect the process to exit with status 0 within [`STOP_TIMEOUT`].
fn run_to_exit(session: &mut Session)
{
    session.resume().expect("resume");
    match next_stop(session) {
        StopEvent::Exited { code: 0 } => {}
        other => panic!("expected a clean exit, got {other:?}"),
    }
    assert_eq!(session.state(), SessionState::Exited);
}

/// The `ps` state of `pid` (for example `S`, `R`, `T` or `Z`), empty if it does not exist.
fn process_state(pid: u32) -> String
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

/// The process exists and is not stopped, sampled a few times.
fn assert_running(pid: u32, what: &str)
{
    for _ in 0..5 {
        let state = process_state(pid);
        assert!(
            !state.is_empty() && !state.starts_with('T') && !state.starts_with('Z'),
            "{what}: the process is stopped or gone (state {state:?})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Attach a fresh session to `target`.
fn attach(target: &Spawned) -> Session
{
    let mut session = Session::new(Box::new(MacOSBackend::new().expect("MacOSBackend::new")));
    session.attach(target.pid()).expect("attach");
    session
}

/// A fresh session attached to the released `target` sees `originals` raw at their
/// addresses, and the target runs freely: no trap is left to stop it. The session is
/// released again on drop.
fn assert_released_clean(target: &Spawned, originals: &[(Address, Vec<u8>)])
{
    let mut fresh = attach(target);
    for (address, original) in originals {
        assert_eq!(&raw(&mut fresh, *address), original, "a trap was left at {address}");
    }
    fresh.resume().expect("resume the re-attached process");
    assert_eq!(
        fresh.wait(Some(QUIET_PERIOD)).expect("wait"),
        None,
        "the released process stopped again"
    );
    drop(fresh);
    assert_running(target.pid().0, "after releasing the re-attached session");
}

/// The trap bytes the engine plants are arm64 `brk #0`; the session view hides them,
/// the raw bytes show them, and removing the breakpoint restores the original bytes.
#[test]
fn breakpoint_writes_trap_and_remove_restores_original_bytes()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let pc = d.read_registers().expect("read_registers").pc;
        let original = raw(&mut d, pc);
        assert_eq!(d.read_memory(pc, 4).expect("session read"), original);

        let id = add(&mut d, pc);
        let with_trap = raw(&mut d, pc);
        let trap = arch::for_architecture(d.architecture())
            .expect("architecture with trap support")
            .trap_instruction();
        assert_eq!(&with_trap[..trap.len()], trap, "trap bytes not written at {pc}");
        assert_eq!(with_trap, TRAP, "arm64 brk #0 not written at {pc}");
        assert_ne!(with_trap, original, "the entry instruction already was a trap");
        assert_eq!(
            d.read_memory(pc, 4).expect("session read with the trap"),
            original,
            "Session::read_memory shows the trap at {pc}"
        );

        d.remove_breakpoint(id).expect("remove_breakpoint");
        assert_eq!(raw(&mut d, pc), original, "original bytes not restored at {pc}");
        assert_eq!(d.read_memory(pc, 4).expect("session read"), original);
        assert!(
            d.breakpoints().is_empty(),
            "breakpoint table not empty: {:?}",
            d.breakpoints()
        );
    });
}

/// A breakpoint on a function is reported with the mapped pc, on the thread that hit
/// it, which becomes selected; the hit is counted and the trap stays in place.
#[test]
fn resume_stops_at_breakpoint_in_a_function()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &["5"]);
        let entry_thread = d.selected_thread().expect("a selected thread at entry");
        let tick = function_address(&mut d, &fixture, "tick");
        let id = add(&mut d, tick);

        d.resume().expect("resume");
        match next_stop(&mut d) {
            StopEvent::Breakpoint { thread, pc } => {
                assert_eq!(pc, tick, "breakpoint stop at the wrong pc");
                assert_eq!(thread, entry_thread, "breakpoint stop on an unexpected thread");
                assert_eq!(d.selected_thread(), Some(thread), "the stopping thread is not selected");
            }
            other => panic!("expected a breakpoint stop at {tick}, got {other:?}"),
        }
        assert_eq!(d.state(), SessionState::Stopped);
        assert_eq!(hits(&d, id), 1);
        assert!(
            matches!(d.last_stop(), Some(StopEvent::Breakpoint { .. })),
            "last_stop: {:?}",
            d.last_stop()
        );
        assert_eq!(d.read_registers().expect("read_registers at the stop").pc, tick);
        assert_eq!(raw(&mut d, tick), TRAP, "the trap is gone after the stop");
    });
}

/// Continuing from a breakpoint stop steps over the trap and the program runs to
/// completion, without the caller calling `wait`. This was the test kept `#[ignore]`d for
/// T07; AC5 (no ignored test names T07 any more) holds now that it runs.
#[test]
fn continue_past_breakpoint_runs_to_completion()
{
    harness::run(|t| {
        let fixture = fixture!("hello");
        let mut d = t.launch(&fixture);
        let main = function_address(&mut d, &fixture, "main");
        let id = add(&mut d, main);

        run_to(&mut d, main);
        assert_eq!(hits(&d, id), 1);

        // `resume` itself steps over the trap and continues: the program runs and prints
        // before anyone calls `wait`.
        d.resume().expect("resume past the breakpoint");
        assert!(
            d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)),
            "fixture output never arrived after continuing (without wait); stdout: {:?}, stderr: {:?}",
            d.stdout(),
            d.stderr()
        );
        assert_eq!(
            d.wait(Some(STOP_TIMEOUT)).expect("wait for the exit"),
            Some(StopEvent::Exited { code: 0 })
        );
    });
}

/// AC1: a breakpoint on `tick` is hit exactly once per loop iteration, the counter
/// argument advances by one each time, `hit_count` counts every hit, and the program
/// then exits normally with the right result.
#[test]
fn breakpoint_in_a_loop_is_hit_on_every_iteration()
{
    const N: u64 = 25;

    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[&N.to_string()]);
        let tick = function_address(&mut d, &fixture, "tick");
        let id = add(&mut d, tick);

        for earlier in 0..N {
            let thread = run_to(&mut d, tick);
            assert_eq!(x0(&mut d, thread), earlier, "counter argument at hit {}", earlier + 1);
            assert_eq!(hits(&d, id), earlier + 1);
        }

        run_to_exit(&mut d);
        let expected = format!("final count: {N}");
        assert!(
            d.wait_for_stdout(&expected, STOP_TIMEOUT),
            "missing {expected:?}; stdout: {:?}",
            d.stdout()
        );
    });
}

/// AC2: `step_instruction` from a breakpoint address executes the original instruction
/// (not the trap) and re-arms the trap; further steps execute exactly one instruction
/// each, every one reported as a `SingleStep` of that thread.
#[test]
fn step_instruction_executes_one_instruction_at_a_time()
{
    const STEPS: usize = 8;

    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let id = add(&mut d, tick);
        let thread = run_to(&mut d, tick);
        assert_eq!(raw(&mut d, tick), TRAP);

        let mut straight = 0;
        for n in 1..=STEPS {
            let before = d.read_registers_for(thread).expect("registers before the step");
            let pc = before.pc.value();
            let insn = instruction_at(&mut d, before.pc);

            d.step_instruction(thread).expect("step_instruction");
            assert_eq!(d.state(), SessionState::Running);
            let event = next_stop(&mut d);
            assert_eq!(
                event,
                StopEvent::SingleStep { thread },
                "step {n} from {pc:#x} ({insn:#010x}) ended with another stop"
            );
            assert_eq!(d.state(), SessionState::Stopped);
            assert_eq!(d.last_stop(), Some(&StopEvent::SingleStep { thread }));
            assert_eq!(d.wait(Some(Duration::ZERO)).expect("wait(0)"), None, "step {n}: extra stop");

            let after = d.read_registers_for(thread).expect("registers after the step");
            let next = after.pc.value();
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

            if n == 1 {
                // The step started on the breakpoint: the original instruction ran (a
                // trap would have reported a breakpoint stop and left the pc in place)...
                assert_ne!(insn.to_le_bytes(), TRAP, "the session view shows the trap");
                if let Some(decrement) = sp_decrement(insn) {
                    assert_eq!(
                        after.sp.value(),
                        before.sp.value() - decrement,
                        "the prologue `sub sp` did not execute"
                    );
                }
                // ...the trap is back, and a step is not a hit.
                assert_eq!(raw(&mut d, tick), TRAP, "the trap was not re-armed after the step");
                assert_eq!(hits(&d, id), 1, "stepping off the breakpoint counted a hit");
            }
        }
        // `tick` starts with a straight-line prologue, so at least those steps prove
        // "exactly one instruction".
        assert!(straight >= 3, "only {straight} of {STEPS} steps were straight-line");

        // The breakpoint still works after the steps.
        run_to(&mut d, tick);
        assert_eq!(hits(&d, id), 2);
    });
}

/// AC3: breakpoints on two consecutive instructions are both hit, in order, on every
/// pass: stepping over the first lands on the second, which is then reported.
#[test]
fn breakpoints_on_consecutive_instructions_are_hit_in_order()
{
    const PASSES: u64 = 4;

    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let first = function_address(&mut d, &fixture, "tick");
        let second = Address::from(first.value() + 4);
        let insn = instruction_at(&mut d, first);
        assert_eq!(flow(first.value(), insn), Flow::Next, "{insn:#010x} at {first} is a branch");

        let first_id = add(&mut d, first);
        let second_id = add(&mut d, second);
        for pass in 0..PASSES {
            let thread = run_to(&mut d, first);
            assert_eq!(x0(&mut d, thread), pass, "counter at pass {pass}");
            let again = run_to(&mut d, second);
            assert_eq!(again, thread, "the second breakpoint was hit by another thread");
            assert_eq!(hits(&d, first_id), pass + 1);
            assert_eq!(hits(&d, second_id), pass + 1);
        }
    });
}

/// AC4: detaching restores the original bytes of every breakpoint in a process that
/// keeps running; a fresh attach reads them back raw and the process runs freely.
#[test]
fn detach_restores_original_bytes_and_leaves_the_process_running()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let target = t.spawn(&fixture, &[LONG_LOOP]);
        let mut session = attach(&target);
        let tick = function_address(&mut session, &fixture, "tick");
        let addresses = [tick, Address::from(tick.value() + 8)];
        let originals: Vec<(Address, Vec<u8>)> = addresses
            .iter()
            .map(|&address| (address, raw(&mut session, address)))
            .collect();
        for &address in &addresses {
            add(&mut session, address);
            assert_eq!(raw(&mut session, address), TRAP);
        }
        run_to(&mut session, tick);

        session.detach().expect("detach");
        assert_eq!(session.state(), SessionState::NotStarted);
        drop(session);
        assert_running(target.pid().0, "after detach");

        assert_released_clean(&target, &originals);
    });
}

/// AC6: writing memory over an armed breakpoint updates its saved original bytes but
/// keeps the trap: the breakpoint is still hit, and removing it leaves the *written*
/// bytes. The written instruction is never executed, and the process is killed.
#[test]
fn write_memory_over_a_breakpoint_keeps_it_armed()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = raw(&mut d, tick);
        assert_ne!(original, NOP, "tick already starts with a nop");
        let id = add(&mut d, tick);

        // Stopped at the entry, not in `tick`: no thread executes the written bytes.
        d.write_memory(tick, &NOP).expect("write over the breakpoint");
        assert_eq!(raw(&mut d, tick), TRAP, "the write replaced the trap");
        assert_eq!(d.read_memory(tick, 4).expect("session read"), NOP, "the session view");
        assert!(d.breakpoint_info(id).expect("breakpoint_info").enabled);

        run_to(&mut d, tick);
        assert_eq!(hits(&d, id), 1);

        d.remove_breakpoint(id).expect("remove_breakpoint");
        assert_eq!(raw(&mut d, tick), NOP, "remove restored the stale original bytes");

        // Its code was changed: never run it on.
        assert_eq!(
            d.kill().expect("kill"),
            StopEvent::Killed {
                signal: ferros_core::SIGKILL
            }
        );
    });
}

/// AC7: dropping a session attached to a spawned process restores the original bytes
/// of a breakpoint in use (stopped at it, or running after continuing from it) and the
/// process keeps running.
#[test]
fn dropping_an_attached_session_restores_original_bytes()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        for running in [false, true] {
            let target = t.spawn(&fixture, &[LONG_LOOP]);
            let mut session = attach(&target);
            let tick = function_address(&mut session, &fixture, "tick");
            let original = raw(&mut session, tick);
            let id = add(&mut session, tick);
            run_to(&mut session, tick);
            assert_eq!(hits(&session, id), 1);
            if running {
                // Continue from the breakpoint (a step-over, then a continue) and drop
                // while the process runs.
                session.resume().expect("resume from the breakpoint");
            }
            drop(session);
            assert_running(target.pid().0, &format!("after the drop (running: {running})"));

            assert_released_clean(&target, &[(tick, original)]);
        }
    });
}

/// AC8: a breakpoint whose trap would overlap another one is refused with
/// `InvalidArgument`, changes nothing, and the first breakpoint still works.
#[test]
fn overlapping_breakpoint_is_refused()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let next = Address::from(tick.value() + 4);
        let next_original = raw(&mut d, next);
        let id = add(&mut d, tick);

        let overlap = Address::from(tick.value() + 2);
        let refused = d.add_breakpoint(BreakpointRequest::Software { address: overlap });
        assert!(
            matches!(refused, Err(DebuggerError::InvalidArgument(_))),
            "an overlapping breakpoint at {overlap} was not refused: {refused:?}"
        );
        assert_eq!(d.breakpoints().len(), 1, "table: {:?}", d.breakpoints());
        assert_eq!(raw(&mut d, tick), TRAP);
        assert_eq!(raw(&mut d, next), next_original, "the refused trap was written");

        let thread = run_to(&mut d, tick);
        assert_eq!(pc_of(&mut d, thread), tick);
        assert_eq!(hits(&d, id), 1);
    });
}

/// A breakpoint on `worker_body` in `threads` is hit once by each worker (two distinct
/// threads, neither the main thread), and the program then runs to completion.
#[test]
fn breakpoint_in_worker_is_hit_once_per_thread()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        let mut d = t.launch(&fixture);
        let main = *d.threads().expect("threads").first().expect("the main thread");
        let worker_body = function_address(&mut d, &fixture, "worker_body");
        let id = add(&mut d, worker_body);

        let first = run_to(&mut d, worker_body);
        let second = run_to(&mut d, worker_body);
        assert_ne!(first, second, "the same thread hit worker_body twice");
        assert_ne!(first, main, "the main thread hit worker_body");
        assert_ne!(second, main, "the main thread hit worker_body");
        assert_eq!(hits(&d, id), 2);

        run_to_exit(&mut d);
        assert!(
            d.wait_for_stdout("threads finished", STOP_TIMEOUT),
            "stdout: {:?}",
            d.stdout()
        );
    });
}

/// A disabled breakpoint is not hit (its original bytes are back) and an enabled one is
/// hit again.
#[test]
fn disabled_breakpoint_is_not_hit_until_enabled()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = raw(&mut d, tick);
        let id = add(&mut d, tick);
        let thread = run_to(&mut d, tick);
        let counter = x0(&mut d, thread);

        d.disable_breakpoint(id).expect("disable");
        assert_eq!(raw(&mut d, tick), original, "disable left the trap");
        d.resume().expect("resume with the breakpoint disabled");
        assert_eq!(
            d.wait(Some(QUIET_PERIOD)).expect("wait"),
            None,
            "the process stopped with the breakpoint disabled"
        );
        d.interrupt().expect("interrupt");
        assert!(
            matches!(next_stop(&mut d), StopEvent::Interrupted { .. }),
            "last_stop: {:?}",
            d.last_stop()
        );
        assert_eq!(hits(&d, id), 1, "a disabled breakpoint counted a hit");

        d.enable_breakpoint(id).expect("enable");
        assert_eq!(raw(&mut d, tick), TRAP, "enable did not re-insert the trap");
        let thread = run_to(&mut d, tick);
        assert_eq!(hits(&d, id), 2);
        assert!(x0(&mut d, thread) > counter, "the loop did not advance while disabled");
    });
}

/// Removing a breakpoint while stopped at it, then resuming, runs to completion.
#[test]
fn removing_the_breakpoint_at_the_stop_runs_to_completion()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &["5"]);
        let tick = function_address(&mut d, &fixture, "tick");
        let original = raw(&mut d, tick);
        let id = add(&mut d, tick);
        run_to(&mut d, tick);

        d.remove_breakpoint(id).expect("remove at the stop");
        assert_eq!(raw(&mut d, tick), original);
        run_to_exit(&mut d);
        assert!(d.wait_for_stdout("final count: 5", STOP_TIMEOUT), "stdout: {:?}", d.stdout());
    });
}

/// `Session::read_memory` shows the original bytes over a range that contains traps,
/// before and after a hit, while the raw bytes show the traps.
#[test]
fn session_read_memory_hides_traps()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &[LONG_LOOP]);
        let tick = function_address(&mut d, &fixture, "tick");
        let start = Address::from(tick.value() - 4);
        let before = d.read_memory(start, 16).expect("read before");
        add(&mut d, tick);
        add(&mut d, Address::from(tick.value() + 8));

        for when in ["after insertion", "at the hit"] {
            if when == "at the hit" {
                run_to(&mut d, tick);
            }
            assert_eq!(d.read_memory(start, 16).expect("session read"), before, "{when}");
            let raw_range = d.backend_mut().read_memory(start, 16).expect("raw read");
            assert_eq!(raw_range[4..8], TRAP, "{when}: no trap at tick");
            assert_eq!(raw_range[12..16], TRAP, "{when}: no trap at tick+8");
            assert_eq!(raw_range[..4], before[..4], "{when}");
            assert_eq!(raw_range[8..12], before[8..12], "{when}");
        }
    });
}

/// A breakpoint added at a thread's current pc is stepped over on resume, not reported
/// (as GDB does): the program runs to completion.
#[test]
fn breakpoint_at_the_current_pc_is_stepped_over()
{
    harness::run(|t| {
        let fixture: Fixture = fixture!("hello");
        let mut d = t.launch(&fixture);
        let pc = d.read_registers().expect("read_registers").pc;
        let id = add(&mut d, pc);
        assert_eq!(hits(&d, id), 0);

        run_to_exit(&mut d);
        assert!(
            d.wait_for_stdout("hello from ferros fixture", STOP_TIMEOUT),
            "stdout: {:?}",
            d.stdout()
        );
    });
}
