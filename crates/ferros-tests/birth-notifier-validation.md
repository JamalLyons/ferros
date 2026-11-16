# Issue #80: internal dyld notifier breakpoint exposed during the birth-trap test

Date: 2026-11-15. Branch `t3/plan-issue-80`, base `5345cdf`, macOS arm64 (Apple M4).

## Captured event and ordering

To catch the failure, a copy of the checkout with temporary diagnostics (not
committed) ran the test binary of
`single_step::thread_created_before_pthread_is_published_stays_at_the_trap`
directly, in 8 parallel streams. The diagnostics did two things:

- Logged every `Breakpoint` stop in `try_consume_birth`, together with the
  birth-policy sites and the bytes in memory at that pc.
- Described the stray stop the test received.

The test failed in 1 of 320 runs, at the assertion `dyld's image notifier was
delivered to the client`. The log for that run (kept locally, not committed)
shows this sequence:

1. `Breakpoint { thread: 3843, pc: 0x104e95d98 }`. Memory held `brk`. The
   policy's only site was `0x104e95d98 entry=false armed=true client=false`
   (the executable-entry notifier). The stop was consumed internally: pthread
   entries were installed, the notifier was retired (original `fd 7b bf a9`
   written back) and the exception was answered.
2. The **same thread** stopped at the **same pc** again: `Breakpoint { thread:
   3843, pc: 0x104e95d98 }`. Memory now held `fd 7b bf a9` (`stp x29, x30,
   [sp, #-16]!`), not `brk`. `notifier=None`. The only sites were the two armed
   pthread entries. No site matched, so the stop was published.
3. The test's 50 ms `wait` returned that stop. Its attribution:
   - image: `ferros-fixture-spawn_thread`, loaded at `0x104e94000`;
   - pc: `LC_MAIN` (`main` + `0x1d98`), the instruction the birth policy trapped;
   - raw instruction at the pc: `0xa9bf7bfd`;
   - thread list: only the main thread, so no newborn existed yet.

**Ownership.** The stop belonged to the engine: it was the birth policy's
notifier, and the client had no breakpoint there. It was raised by an instruction
that was no longer in memory. Of the five candidates in the plan, this is
candidate 3: a trap fires after its record is gone, because the core executes a
stale copy of the instruction. The other candidates are ruled out by the same
log:

- **(1) Not a misread original.** `client=false` on the first stop, and the
  notifier's recorded original was `fd 7b bf a9`.
- **(2) Not a double install.** Both entry sites were `client=false`, and the
  stray pc is not an entry.
- **(4) Not a bypassed classifier.** The stop went through `try_consume_birth`.
- **(5) Not a real stop.** Memory at the pc holds no `brk`, and neither the
  client nor the test put one there.

## Mechanism

`write_memory` (`crates/ferros-macos/src/memory.rs`) wrote target code with
`vm_write` and nothing else. On arm64 that updates memory but not the
instruction caches of the cores the target runs on. A thread can therefore keep
executing the instruction its core cached before the write.

XNU synchronises the instruction cache for a page only once, in
`vm_fault_enter_set_mapped`: the first time the page is mapped executable
(`!m->vmp_xpmapped`). Later writes are not synchronised; the kernel leaves that
to "higher layers". LLDB's debugserver does that: after every `mach_vm_write`
in `MachVMMemory::WriteRegion`, it calls `vm_machine_attribute(task, addr,
count, MATTR_CACHE, MATTR_VAL_CACHE_FLUSH)`. Ferros never did.

The notifier path hits this window more often than an ordinary breakpoint does.
`consume_image_notifier` restores the original instruction and answers the
exception in the same pass, so the trapped thread re-executes the restored
address microseconds after the write. A client breakpoint step-over takes
milliseconds between restore and resume. Under load, that thread sometimes
fetched the cached `brk` a second time.

Two standalone C probes (also not committed) confirm both directions against a
spinning child process:

- **Restore direction.** The parent plants a flushed `brk`, takes the trap,
  restores the original, answers and resumes.
  - Without a flush: with 1, 4 and 10 spinning threads, 195/2000, 468/2000 and
    1738/2000 rounds trapped again on the restored address.
  - With `MATTR_VAL_CACHE_FLUSH`: 0, 0 and 3/2000 rounds. The 3 are threads that
    had already fetched the `brk` before the restore.
  - With 8 threads and no flush, a delay between restore and reply cut the stale
    traps from 953/1000 rounds (no delay) to 1 (50 µs) and 0 (200 µs or more).
    This is the timing difference between the notifier path and a client
    step-over.
- **Plant direction.** A `brk` written over an instruction the threads have
  cached is skipped.
  - Without a flush: with 1, 4 and 10 threads, 22/2000, 1355/2000 and 546/2000
    rounds ran more calls through the replaced instruction than there are
    threads. The worst round ran 168,888,263 calls.
  - With the flush: 0 rounds in every configuration.

## Fix

`write_memory` now flushes the written range after the `vm_write` succeeds
(`flush_caches`). It calls `mach_vm_machine_attribute(task, addr, size,
MATTR_CACHE, MATTR_VAL_CACHE_FLUSH)` once per page, because XNU's
`pmap_attribute_cache_sync` handles one page per call. A failed flush is
returned as an error, so a write is never reported as done while the target may
still execute stale code.

Every target write goes through this function, so the fix also covers:

- engine software breakpoints (`write_target_memory`);
- the birth traps, their retirement and re-arming (`birth::patch`);
- the emulated entry frame.

The birth policy, its classification and the test's event assumptions are
unchanged. They were correct; the core was not executing what they wrote.

## Regression

`tests/code_writes.rs::planted_trap_is_executed_once_the_target_runs` launches
the new `hot_loop` fixture with 8 spinning threads. In each of 100 rounds:

1. Let the threads spin through `spin_site`, so it is cached on every core.
2. Interrupt and read the fixture's call counter.
3. Plant a `brk` on `spin_site` and resume.
4. At the reported trap, require the counter to have advanced by at most the
   number of threads (each may finish the call it was inside).

Without the fix it failed at round 1: `21958 calls ran through the instruction
replaced at 0x000000010269839c`. With the fix it passes. The test exercises the
plant direction because that is deterministic: every round fails without the
flush, while the restore direction needs a sub-millisecond reply window that the
backend's public API cannot produce. Both directions are the same missing flush
in the same function.

`thread_created_before_pthread_is_published_stays_at_the_trap` now explains an
unexpected stop when it fails (`describe_stray_stop`): its image, whether it is
the executable entry, and the bytes in memory there.

## Verification

All runs on macOS arm64 (Apple M4).

- **Unfixed:** 320 direct runs of the birth-trap test in 8 parallel streams. 319
  passed; 1 failed with the stray notifier stop captured above.
- **Fixed:** the same diagnostic copy with only `memory.rs` replaced by the fix,
  960 direct runs in 8 parallel streams. 953 passed, 7 failed, 0 stray stops.
  The 953 passing runs each logged one notifier stop, consumed internally. The 7
  failures were the first iteration of streams 1 to 7, each with `task_for_pid
  was denied`: the streams started at the same moment and each re-signed the
  same test binary. Every later iteration passed. The signing race is the likely
  cause, not a proven one; it is unrelated to the stray stop.
- **Full `single_step` suites and full integration suites:** 12 runs of `cargo
  test -p ferros-tests --test single_step` (32 tests each) while 2 `just
  test-integration` runs ran in parallel. All exited 0.
- **Native regression:** fails without the fix (round 1) and passes with it.
- **Checks:** `just fmt-check`, `just lint`, `just test`, `just
  test-integration` and `just check-boundary` all exit 0.
- **Leaks:** `pgrep -fl ferros-fixture-` printed nothing and exited 1 after the
  runs.
