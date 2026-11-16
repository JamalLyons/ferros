//! Code the macOS backend writes is the code the target executes next (issue #80).
//!
//! `vm_write` changes memory, not the instruction caches of the cores the target runs
//! on. Without a cache flush, a thread keeps executing the instruction it cached: it runs
//! past a newly planted `brk`, or traps on a `brk` that has been replaced. The second
//! is how the birth policy's executable entry trap reached the client: the trap was
//! restored and the thread answered at once, and its next fetch was the stale `brk`.
//! These tests drive the backend directly (`d.session_mut().backend_mut()`), like
//! `single_step.rs`, against `hot_loop`, whose threads keep one function hot on every
//! core they run on.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{Address, Backend, ResumeRequest, StopEvent};
use ferros_tests::arm64::TRAP;
use ferros_tests::fixture;
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::symbols::function_address;

/// How long a test waits for a stop it expects.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Spinning threads in `hot_loop`, main included.
const THREADS: u64 = 8;

/// Plant cycles. Without the cache flush nearly every cycle fails.
const ROUNDS: usize = 100;

/// The session's backend.
fn backend(d: &mut Debuggee) -> &mut dyn Backend
{
    d.session_mut().backend_mut()
}

/// Interrupt the running target and drain the stop.
fn interrupt(d: &mut Debuggee)
{
    backend(d).interrupt().expect("interrupt");
    match backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt") {
        Some(StopEvent::Interrupted { .. }) => {}
        other => panic!("expected the interrupt, got {other:?}"),
    }
}

/// The address `hot_loop` printed for its call counter.
fn calls_address(d: &Debuggee) -> Address
{
    assert!(
        d.wait_for_stdout("calls=", STOP_TIMEOUT),
        "hot_loop did not print its counter"
    );
    let stdout = d.stdout();
    let hex = stdout
        .lines()
        .find_map(|line| line.strip_prefix("calls=0x"))
        .expect("the counter line");
    Address::from(u64::from_str_radix(hex.trim(), 16).expect("a hex address"))
}

fn read_u64(d: &mut Debuggee, address: Address) -> u64
{
    let bytes = backend(d).read_memory(address, 8).expect("read the counter");
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

/// A `brk` written while the target is stopped is executed as soon as it runs: no thread
/// completes another call through the instruction it replaced.
///
/// Each running thread can finish at most the one call it was inside when the target
/// stopped. A thread that keeps calling `spin_site` after the resume executed a stale
/// copy of its first instruction.
#[test]
fn planted_trap_is_executed_once_the_target_runs()
{
    harness::run(|t| {
        let fixture = fixture!("hot_loop");
        let mut d = t.launch_with_args(&fixture, &[&THREADS.to_string()]);
        let site = function_address(d.session_mut(), &fixture, "spin_site");
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("start the threads");
        let calls = calls_address(&d);
        interrupt(&mut d);
        let original = backend(&mut d).read_memory(site, TRAP.len()).expect("read spin_site");
        for round in 0..ROUNDS {
            // Let every thread spin through the original again, so it is cached.
            backend(&mut d)
                .resume(&ResumeRequest::continue_all())
                .expect("run the original");
            std::thread::sleep(Duration::from_millis(2));
            interrupt(&mut d);
            let before = read_u64(&mut d, calls);
            backend(&mut d).write_memory(site, &TRAP).expect("plant the trap");
            backend(&mut d)
                .resume(&ResumeRequest::continue_all())
                .expect("run to the trap");
            match backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the trap") {
                Some(StopEvent::Breakpoint { pc, .. }) if pc == site => {}
                other => panic!("round {round}: expected a breakpoint at {site}, got {other:?}"),
            }
            let ran = read_u64(&mut d, calls) - before;
            assert!(
                ran <= THREADS,
                "round {round}: {ran} calls ran through the instruction replaced at {site}"
            );
            // Threads that trapped in the same batch are reported with no resume between.
            while let Some(event) = backend(&mut d).wait(Some(Duration::ZERO)).expect("drain") {
                assert!(
                    matches!(event, StopEvent::Breakpoint { pc, .. } if pc == site),
                    "round {round}: unexpected stop {event:?}"
                );
            }
            backend(&mut d).write_memory(site, &original).expect("restore spin_site");
        }
    });
}
