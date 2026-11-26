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

use ferros_core::{Address, Backend, BackendError, ResumeRequest, StopEvent, ThreadAction, ThreadId};
use ferros_macos::{MacOSBackend, diagnostics};
use ferros_tests::arm64::TRAP;
use ferros_tests::harness::{self, Debuggee};
use ferros_tests::symbols::function_address;
use ferros_tests::{Fixture, fixture};

/// How long a test waits for a stop it expects.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Spinning threads in `hot_loop`, main included.
const THREADS: usize = 8;

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

/// The address `hot_loop` printed for its array of worker counters.
fn calls_address(d: &Debuggee) -> Address
{
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if let Some(hex) = d
            .stdout()
            .split_inclusive('\n')
            .find_map(|line| line.strip_prefix("calls=0x")?.strip_suffix('\n'))
        {
            return Address::from(u64::from_str_radix(hex, 16).expect("a hex address"));
        }
        assert!(Instant::now() < deadline, "hot_loop did not print its counters");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn read_calls(d: &mut Debuggee, address: Address) -> [u64; THREADS]
{
    let bytes = backend(d)
        .read_memory(address, THREADS * size_of::<u64>())
        .expect("read the worker counters");
    let (words, _) = bytes.as_chunks::<{ size_of::<u64>() }>();
    std::array::from_fn(|index| u64::from_le_bytes(words[index]))
}

/// Let each worker execute both restored entry points before planting the next trap.
/// Two increments require a fresh call even if a worker was already inside either
/// function when the round started. A worker whose previous trap is still waiting
/// for a kernel reply cannot advance its counter.
fn run_original(d: &mut Debuggee, calls: Address, previous: Option<Address>, round: usize) -> [u64; THREADS]
{
    let baseline = read_calls(d, calls);
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        let mut stop = match backend(d).resume(&ResumeRequest::continue_all()) {
            Ok(()) => {
                let stop = backend(d)
                    .wait(Some(Duration::from_millis(1)))
                    .expect("wait while running the original");
                if stop.is_some() {
                    stop
                } else {
                    backend(d).interrupt().expect("interrupt the original");
                    backend(d).wait(Some(STOP_TIMEOUT)).expect("wait for the interrupt")
                }
            }
            Err(BackendError::StopPending) => backend(d).wait(Some(Duration::ZERO)).expect("take the pending stop"),
            Err(err) => panic!("round {round}: resume the original: {err}"),
        };
        assert!(stop.is_some(), "round {round}: no stop while running the original");
        // A worker that trapped in the previous round can publish late (#122).
        // Drain those stops and keep running until every worker has passed both sites.
        while let Some(event) = stop {
            assert!(
                matches!(event, StopEvent::Interrupted { .. })
                    || matches!(event, StopEvent::Breakpoint { pc, .. } if Some(pc) == previous),
                "round {round}: expected an interrupt or a late trap at {previous:?}, got {event:?}"
            );
            stop = backend(d).wait(Some(Duration::ZERO)).expect("drain");
        }
        let current = read_calls(d, calls);
        if current.iter().zip(baseline).all(|(&now, before)| now - before >= 2) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "round {round}: workers did not all advance through original code: {baseline:?} -> {current:?}"
        );
    }
}

/// A `brk` written while the target is stopped is executed as soon as it runs: no thread
/// completes another call through the instruction it replaced.
///
/// Each running thread can finish at most the one call it was inside when the target
/// stopped. A thread that keeps calling either function after the resume executed a
/// stale copy of the replaced instruction.
#[test]
fn planted_trap_is_executed_once_the_target_runs()
{
    harness::run(|t| {
        let fixture = fixture!("hot_loop");
        let mut d = t.launch_with_args(&fixture, &[&THREADS.to_string(), "--code-writes"]);
        let sites = ["counted_site", "counted_site_alternate"].map(|name| function_address(d.session_mut(), &fixture, name));
        assert_ne!(sites[0], sites[1]);
        let originals = sites.map(|site| backend(&mut d).read_memory(site, TRAP.len()).expect("read the original"));
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("start the threads");
        let calls = calls_address(&d);
        interrupt(&mut d);
        let mut previous = None;
        for round in 0..ROUNDS {
            let slot = round % sites.len();
            let site = sites[slot];
            let before = run_original(&mut d, calls, previous, round);
            // A late exception from the previous plant has a different PC.
            backend(&mut d).write_memory(site, &TRAP).expect("plant the trap");
            backend(&mut d)
                .resume(&ResumeRequest::continue_all())
                .expect("run to the trap");
            match backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the trap") {
                Some(StopEvent::Breakpoint { pc, .. }) if pc == site => {}
                other => panic!("round {round}: expected a breakpoint at {site}, got {other:?}"),
            }
            for (worker, (after, before)) in read_calls(&mut d, calls).into_iter().zip(before).enumerate() {
                let ran = after - before;
                assert!(
                    ran <= 1,
                    "round {round}: worker {worker} made {ran} calls through the replaced instruction"
                );
            }
            // Threads that trapped in the same batch are reported with no resume between.
            while let Some(event) = backend(&mut d).wait(Some(Duration::ZERO)).expect("drain") {
                assert!(
                    matches!(event, StopEvent::Breakpoint { pc, .. } if pc == site),
                    "round {round}: unexpected stop {event:?}"
                );
            }
            backend(&mut d)
                .write_memory(site, &originals[slot])
                .expect("restore the original");
            previous = Some(site);
        }
    });
}

/// #335: repeatedly resume onto the same trap to exercise the race between the
/// exception loop's new suspension and the preceding `task_resume`.
#[test]
fn immediate_retrap_keeps_the_task_suspended()
{
    harness::run(|t| {
        let fixture = fixture!("hot_loop");
        let mut d = t.launch_with_args(&fixture, &["1"]);
        let site = function_address(d.session_mut(), &fixture, "spin_site");
        backend(&mut d).write_memory(site, &TRAP).expect("plant the trap");
        let request = ResumeRequest::continue_all();
        for round in 0..20_000 {
            backend(&mut d).resume(&request).unwrap_or_else(|err| {
                let counts = diagnostics::process_suspend_counts(d.pid()).map(|counts| counts.task);
                panic!("round {round}: {err}; task suspend count {counts:?}");
            });
            let stop = backend(&mut d).wait(Some(STOP_TIMEOUT)).expect("wait for the trap");
            assert!(
                matches!(stop, Some(StopEvent::Breakpoint { pc, .. }) if pc == site),
                "round {round}: {stop:?}"
            );
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

#[test]
fn short_snapshot_refuses_the_write_before_storing_any_bytes()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch(&fixture);
        let tick = function_address(d.session_mut(), &fixture, "tick");
        let before = raw_word(&d, tick);
        assert_ne!(before, TRAP);

        let result = diagnostics::write_with_short_snapshot(d.pid(), tick, &TRAP);
        assert_eq!(raw_word(&d, tick), before, "short snapshot allowed a write: {result:?}");
        let error = result.expect_err("a short snapshot must fail the write").to_string();
        assert!(error.contains("rollback snapshot read 3 of 4 bytes"), "{error}");
        assert!(!error.contains("restored"), "{error}");
    });
}

#[test]
fn unreadable_snapshot_refuses_a_write_to_a_protected_page()
{
    harness::run(|t| {
        let mut d = t.launch_with_args(&fixture!("soft_faults"), &["--guard-page"]);
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("run to the guard page");
        assert!(d.wait_for_stdout("\n", STOP_TIMEOUT), "fixture printed no guard page");
        interrupt(&mut d);
        let stdout = d.stdout();
        let hex = stdout
            .lines()
            .find_map(|line| line.strip_prefix("boundary: 0x"))
            .expect("guard page address line");
        let guard = Address::from(u64::from_str_radix(hex, 16).expect("guard page address"));

        let error = backend(&mut d)
            .write_memory(guard, &[0xa5])
            .expect_err("an unreadable snapshot must fail the write");
        assert!(
            matches!(error, BackendError::PermissionDenied(ref message) if message.contains("not readable")),
            "{error}"
        );
    });
}

/// The four bytes at `address` as they are in memory, including traps the backend hides
/// from `read_memory`.
fn raw_word(d: &Debuggee, address: Address) -> Vec<u8>
{
    diagnostics::raw_memory(d.pid(), address, 4).unwrap_or_else(|err| panic!("read the raw bytes at {address}: {err}"))
}

/// Stop the creator of `spawn_thread` at `spawn_later`, then run only it until its
/// newborn is enumerated and held on a birth trap. Returns the thread and trap address,
/// with the target stopped.
fn hold_a_newborn(d: &mut Debuggee, fixture: &Fixture) -> (ThreadId, Address)
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
                return (newborn, pc);
            }
        }
        assert!(
            Instant::now() < deadline,
            "no newborn held on a birth trap within {STOP_TIMEOUT:?}, stdout: {}",
            d.stdout()
        );
    }
}

#[cfg(target_arch = "aarch64")]
fn assert_entry_write_leaves_thread_unchanged(fault: diagnostics::BirthEntryWrite)
{
    harness::run(move |t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (thread, entry) = hold_a_newborn(&mut d, &fixture);
        let before = backend(&mut d).read_registers(thread).expect("read the newborn registers");
        assert_eq!(before.pc, entry);
        assert_eq!(before.general.len(), 31);
        assert_eq!(before.vector.len(), 32);
        assert!(before.floating.fpsr.is_some() && before.floating.fpcr.is_some());

        // Make the store observable even if the kernel zeroed the new stack.
        let stack = Address::from(before.sp.value() - 16);
        let sentinel = [0xa5; 16];
        backend(&mut d).write_memory(stack, &sentinel).expect("seed the entry frame");

        let port = u32::try_from(thread.raw()).expect("Mach thread port");
        let error = diagnostics::emulate_birth_entry_with_fault(d.pid(), port, fault)
            .expect_err("the injected entry write must fail")
            .to_string();
        assert!(error.contains(&format!("injected {fault:?} write failure")), "{error}");

        let after = backend(&mut d).read_registers(thread).expect("read the registers again");
        let after_stack = diagnostics::raw_memory(d.pid(), stack, sentinel.len()).expect("read the entry frame");
        assert_eq!(after.vector, before.vector, "NEON registers changed");
        assert_eq!(after.floating, before.floating, "floating-point state changed");
        assert_eq!(
            (after.pc, after.sp, after.fp, after.status, after.general, after_stack),
            (
                before.pc,
                before.sp,
                before.fp,
                before.status,
                before.general,
                sentinel.to_vec()
            ),
            "{fault:?} failure changed the general registers or entry frame"
        );
    });
}

#[cfg(target_arch = "aarch64")]
#[test]
fn failed_birth_entry_stack_write_leaves_the_thread_unchanged()
{
    assert_entry_write_leaves_thread_unchanged(diagnostics::BirthEntryWrite::Stack);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn failed_birth_entry_general_register_write_leaves_the_thread_unchanged()
{
    assert_entry_write_leaves_thread_unchanged(diagnostics::BirthEntryWrite::GeneralRegisters);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn failed_birth_entry_neon_write_leaves_the_thread_unchanged()
{
    assert_entry_write_leaves_thread_unchanged(diagnostics::BirthEntryWrite::NeonRegisters);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn failed_birth_entry_rollback_preserves_the_neon_error_category()
{
    use diagnostics::BirthEntryWrite;
    use ferros_core::DebuggerError;

    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (thread, _) = hold_a_newborn(&mut d, &fixture);
        let port = u32::try_from(thread.raw()).expect("Mach thread port");

        let error = diagnostics::emulate_birth_entry_with_faults(d.pid(), port, |write| match write {
            BirthEntryWrite::NeonRegisters => Err(BackendError::PermissionDenied("injected NEON write failure".into())),
            BirthEntryWrite::RestoreGeneralRegisters => Err(BackendError::Os {
                code: 4,
                message: "injected general register rollback failure".into(),
            }),
            _ => Ok(()),
        })
        .expect_err("the injected entry write must fail");

        let DebuggerError::PermissionDenied(message) = error else {
            panic!("expected the original permission error, got {error:?}");
        };
        assert!(message.contains("injected NEON write failure"), "{message}");
        assert!(message.contains("restoring the original general registers"), "{message}");
        assert!(
            message.contains("also failed (OS error 4: injected general register rollback failure)"),
            "{message}"
        );
    });
}

#[cfg(target_arch = "aarch64")]
#[test]
fn wait_preserves_the_continuing_birth_entry_error_category()
{
    use ferros_tests::arm64::NOP;

    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let mut d = t.launch(&fixture);
        let (thread, entry) = hold_a_newborn(&mut d, &fixture);
        let newborn = backend(&mut d).read_registers(thread).expect("read the newborn registers");
        let original_entry = backend(&mut d).read_memory(entry, TRAP.len()).expect("read the entry");
        backend(&mut d).write_memory(entry, &NOP).expect("replace the saved entry");
        assert_eq!(raw_word(&d, entry), TRAP, "the entry must stay armed");

        // A thread already at an armed entry steps off it during resume. Branch there
        // after resume instead, as a continuing thread re-entering a workqueue would.
        let branch = function_address(d.session_mut(), &fixture, "spawn_later");
        let original_branch = backend(&mut d).read_memory(branch, TRAP.len()).expect("read spawn_later");
        let br_x16 = 0xD61F_0200_u32.to_le_bytes();
        backend(&mut d).write_memory(branch, &br_x16).expect("plant br x16");
        let mut registers = newborn.clone();
        registers.pc = branch;
        registers.general[16] = entry.value();
        backend(&mut d)
            .write_registers(thread, &registers)
            .expect("redirect the newborn");

        let request = ResumeRequest::all(ThreadAction::Suspend).with_thread(thread, ThreadAction::Continue);
        backend(&mut d)
            .resume(&request)
            .expect("continue the newborn with birth traps armed");
        let error = backend(&mut d)
            .wait(Some(STOP_TIMEOUT))
            .expect_err("emulating the replaced entry must fail");
        let BackendError::Unsupported(message) = error else {
            panic!("expected Unsupported from entry emulation, got {error:?}");
        };
        assert!(
            message.contains(&format!("thread {} could not pass its pthread entry", thread.raw())),
            "{message}"
        );
        assert!(message.contains("not the stp this backend emulates"), "{message}");

        interrupt(&mut d);
        backend(&mut d)
            .write_memory(branch, &original_branch)
            .expect("restore spawn_later");
        backend(&mut d)
            .write_memory(entry, &original_entry)
            .expect("restore the saved entry");
        backend(&mut d)
            .write_registers(thread, &newborn)
            .expect("restore the newborn registers");
        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("continue after restoring the entry");
        assert!(d.wait_for_stdout("workers finished", STOP_TIMEOUT), "stdout: {}", d.stdout());
    });
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

        let (_, site) = hold_a_newborn(&mut d, &fixture);
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

#[test]
fn retried_non_breakpoint_write_releases_client_ownership_of_a_birth_trap()
{
    harness::run(|t| {
        let fixture = fixture!("spawn_thread");
        let backend_handle = MacOSBackend::new().expect("backend");
        let fail_store = backend_handle.memory_store_fault_injector();
        let mut d = t.launch_with_backend(&fixture, &[], backend_handle);

        let (_, entry) = hold_a_newborn(&mut d, &fixture);
        let original = backend(&mut d).read_memory(entry, TRAP.len()).expect("read the entry");
        assert_ne!(original, TRAP);
        backend(&mut d).write_memory(entry, &TRAP).expect("plant a client breakpoint");

        fail_store();
        let error = backend(&mut d)
            .write_memory(entry, &original)
            .expect_err("the first restore must fail in the store");
        assert!(error.to_string().contains("injected memory store failure"), "{error}");
        assert_eq!(
            backend(&mut d).read_memory(entry, TRAP.len()).expect("read after failure"),
            TRAP,
            "the failed restore lost client ownership"
        );

        backend(&mut d).write_memory(entry, &original).expect("retry the restore");
        assert_eq!(
            backend(&mut d).read_memory(entry, TRAP.len()).expect("read after retry"),
            original
        );
        assert_eq!(raw_word(&d, entry), TRAP, "the birth trap must stay armed until resume");

        backend(&mut d)
            .resume(&ResumeRequest::continue_all())
            .expect("continue and disarm the birth trap");
        let stop = backend(&mut d)
            .wait(Some(STOP_TIMEOUT))
            .expect("wait for the fixture to exit");
        assert!(
            matches!(stop, Some(StopEvent::Exited { code: 0 })),
            "unexpected stop: {stop:?}"
        );
    });
}
