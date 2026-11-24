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

use std::time::{Duration, Instant};

use ferros_core::{Address, Backend, ResumeRequest, StopEvent, ThreadAction};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::arm64::TRAP;
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture};

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

/// Interrupt the running target and drain every stop until it is stopped with nothing
/// pending, accepting a breakpoint at `site` that raced the interrupt (issue #122).
///
/// A thread can execute the `brk` planted at `site` in one round without its exception
/// reaching the backend before that round's drain. The exception is then delivered
/// after the next resume, as `Breakpoint` at `site`, and the interrupt loses the race
/// to it. That thread really executed the planted trap, so the stop is not an error.
/// Any other stop still fails the test.
fn interrupt_racing_trap(d: &mut Debuggee, site: Address, round: usize)
{
    backend(d).interrupt().expect("interrupt");
    let mut stop = backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt");
    assert!(
        stop.is_some(),
        "round {round}: no stop within {STOP_TIMEOUT:?} of the interrupt"
    );
    while let Some(event) = stop {
        assert!(
            matches!(event, StopEvent::Interrupted { .. })
                || matches!(event, StopEvent::Breakpoint { pc, .. } if pc == site),
            "round {round}: expected the interrupt or a breakpoint at {site}, got {event:?}"
        );
        stop = backend(d).wait(Some(Duration::ZERO)).expect("drain");
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
            interrupt_racing_trap(&mut d, site, round);
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

/// A write the backend refuses changes nothing: `Backend::write_memory` is all or
/// nothing (issue #133), which the session's trap rollback relies on. The write starts
/// in the last bytes of the code region that holds `tick` and runs past its end, so part
/// of it would land in the region's own (read-only, code) page.
#[test]
fn refused_write_across_a_code_region_end_leaves_memory_unchanged()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch(&fixture);
        let tick = function_address(d.session_mut(), &fixture, "tick");
        let region = backend(&mut d)
            .memory_regions()
            .expect("memory regions")
            .into_iter()
            .find(|region| region.start <= tick && tick < region.end)
            .expect("the region of tick");
        assert!(region.permissions.contains('x'), "{region:?}");
        let tail = Address::from(region.end.value() - 2);
        let before = backend(&mut d).read_memory(tail, 2).expect("read the region's last bytes");
        backend(&mut d)
            .write_memory(tail, &TRAP)
            .expect_err("a write past the region end is refused");
        assert_eq!(backend(&mut d).read_memory(tail, 2).expect("read again"), before);
        // A write inside the region still succeeds and can be undone.
        let original = backend(&mut d).read_memory(tick, TRAP.len()).expect("read tick");
        backend(&mut d).write_memory(tick, &TRAP).expect("plant a trap");
        assert_eq!(backend(&mut d).read_memory(tick, TRAP.len()).unwrap(), TRAP);
        backend(&mut d).write_memory(tick, &original).expect("restore tick");
    });
}

/// The four bytes at `address` as they are in memory, including traps the backend hides
/// from `read_memory`.
fn raw_word(d: &Debuggee, address: Address) -> Vec<u8>
{
    diagnostics::raw_memory(d.pid(), address, 4).unwrap_or_else(|err| panic!("read the raw bytes at {address}: {err}"))
}

/// Stop the creator of `spawn_thread` at `spawn_later`, then run only it until its
/// newborn is enumerated and held on a birth trap. Returns that trap's address, with
/// the target stopped.
fn hold_a_newborn(d: &mut Debuggee, fixture: &Fixture) -> Address
{
    // Stop the creator at `spawn_later`, then let only it run so the newborn is held.
    let spawn_later = function_address(d.session_mut(), fixture, "spawn_later");
    let entry = backend(d).read_memory(spawn_later, TRAP.len()).expect("read spawn_later");
    backend(d).write_memory(spawn_later, &TRAP).expect("plant a trap");
    backend(d).resume(&ResumeRequest::continue_all()).expect("continue");
    let creator = match backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for spawn_later") {
        Some(StopEvent::Breakpoint { thread, pc }) if pc == spawn_later => thread,
        other => panic!("expected a breakpoint at spawn_later, got {other:?}"),
    };
    while backend(d).wait(Some(Duration::ZERO)).expect("drain").is_some() {}
    backend(d).write_memory(spawn_later, &entry).expect("restore spawn_later");
    let known = backend(d).threads().expect("threads");
    // Run only the creator until its newborn is enumerated and held on a birth trap,
    // rather than for a fixed time a busy machine might not honor.
    let deadline = Instant::now() + STOP_TIMEOUT;
    let mut newborn = None;
    loop {
        // A newborn seen before it reached the trap runs too, so it can get there.
        let mut request = ResumeRequest::all(ThreadAction::Suspend).with_thread(creator, ThreadAction::Continue);
        if let Some(newborn) = newborn {
            request = request.with_thread(newborn, ThreadAction::Continue);
        }
        backend(d).resume(&request).expect("resume the creator");
        std::thread::sleep(Duration::from_millis(50));
        interrupt(d);
        newborn = backend(d)
            .threads()
            .expect("threads")
            .into_iter()
            .find(|id| !known.contains(id));
        if let Some(newborn) = newborn {
            let pc = backend(d).read_registers(newborn).expect("newborn registers").pc;
            if raw_word(d, pc) == TRAP {
                return pc;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no newborn held on a birth trap within {STOP_TIMEOUT:?}, stdout: {}",
            d.stdout()
        );
    }
}

/// A write the backend refuses after planning it leaves a covered birth trap's saved
/// instruction and client-breakpoint ownership as they were, and a later successful
/// write is what disarm keeps.
///
/// `spawn_thread` starts a worker from `spawn_later` while only the creator runs, so its
/// newborn is held on the birth trap at `_thread_start`. Two engine writes over that
/// trap then fail inside the store: a `brk` (which would make the site the engine's
/// breakpoint) and bytes straddling the instruction (which would replace the saved
/// instruction). Neither may change what `read_memory` shows there. The `brk` is then
/// written for real; the continue that disarms the birth traps leaves it in place, so
/// the newborn stops on it.
#[test]
fn failed_overlapping_birth_write_keeps_the_saved_instruction()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let backend_handle = MacOSBackend::new().expect("backend");
        let fail_store = backend_handle.memory_store_fault_injector();
        let fail_store_again = backend_handle.memory_store_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[], backend_handle);

        let site = hold_a_newborn(&mut d, &fixture);
        let saved = backend(&mut d).read_memory(site, 4).expect("read the entry");
        assert_ne!(saved, TRAP, "read_memory shows the birth trap instead of the entry");

        // A failed `brk` over the trap does not make it the engine's breakpoint.
        fail_store();
        backend(&mut d)
            .write_memory(site, &TRAP)
            .expect_err("the injected store failure is reported");
        assert_eq!(
            backend(&mut d).read_memory(site, 4).expect("read after the failed brk"),
            saved,
            "a refused brk changed the client-breakpoint ownership of the birth site"
        );
        assert_eq!(raw_word(&d, site), TRAP, "a refused write removed the birth trap");

        // A failed write straddling the instruction does not replace the saved one.
        let straddle = Address::from(site.value() - 2);
        let before = backend(&mut d).read_memory(straddle, 8).expect("read around the entry");
        fail_store_again();
        backend(&mut d)
            .write_memory(straddle, &[0x5a; 4])
            .expect_err("the injected store failure is reported");
        assert_eq!(
            backend(&mut d).read_memory(straddle, 8).expect("read after the failed write"),
            before,
            "a refused write changed the saved instruction of the birth site"
        );
        assert_eq!(raw_word(&d, site), TRAP, "a refused write removed the birth trap");

        // The same straddling write retried succeeds: it updates only the bytes it
        // covers, including the saved instruction, and the birth trap stays armed.
        backend(&mut d)
            .write_memory(straddle, &[0x5a; 4])
            .expect("the retried straddling write is stored");
        let mut expected = before.clone();
        expected[..4].copy_from_slice(&[0x5a; 4]);
        assert_eq!(
            backend(&mut d)
                .read_memory(straddle, 8)
                .expect("read after the retried write"),
            expected,
            "a successful straddling write did not update the saved instruction"
        );
        assert_eq!(raw_word(&d, site), TRAP, "a successful write removed the birth trap");
        backend(&mut d)
            .write_memory(straddle, &before[..4])
            .expect("undo the straddling write");
        assert_eq!(
            backend(&mut d).read_memory(straddle, 8).expect("read after the undo"),
            before,
            "undoing the straddling write did not restore the saved instruction"
        );

        // The same `brk` retried succeeds. Disarming keeps it: the newborn stops on it.
        backend(&mut d)
            .write_memory(site, &TRAP)
            .expect("plant a breakpoint on the entry");
        assert_eq!(backend(&mut d).read_memory(site, 4).expect("read the breakpoint"), TRAP);
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("continue, disarming the birth traps");
        match backend(&mut d)
            .wait(Some(STOP_TIMEOUT))
            .expect("wait for the entry breakpoint")
        {
            Some(StopEvent::Breakpoint { pc, .. }) if pc == site => {}
            other => panic!("expected the engine breakpoint at {site}, got {other:?}"),
        }
        while backend(&mut d).wait(Some(Duration::ZERO)).expect("drain").is_some() {}
        assert_eq!(raw_word(&d, site), TRAP, "disarm did not keep the engine breakpoint");
        backend(&mut d).write_memory(site, &saved).expect("restore the entry");
        assert_eq!(raw_word(&d, site), saved, "the entry was not restored");
        backend(&mut d).resume(&ResumeRequest::continue_all()).expect("finish");
        assert!(
            d.wait_for_stdout("workers finished", STOP_TIMEOUT),
            "the fixture did not finish after the entry was restored, stdout: {}",
            d.stdout()
        );
    });
}
