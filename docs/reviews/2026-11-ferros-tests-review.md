# ferros-tests review (2026-11)

Adversarial review of `crates/ferros-tests` at trunk `37efda7`. Scope: the harness (`src/`), every
test file under `tests/`, the fixtures and `build.rs`, the cargo runner and CI. Four read-only
reviewer passes (lifecycle and signals; stepping and breakpoints; mocks, symbols and unwinding;
harness, CLI and build) fed this document. Every finding marked **verified** below was checked by
reading the code, and the mutation and timing claims were re-run by hand on this machine. Findings
marked **unverified** depend on macOS scheduling or kernel behaviour that could not be reproduced
here.

Machine: arm64 Mac, 16 logical CPUs, macOS 27.2 (Darwin 27.2.0), Rust 1.100.0, warm `target/`.
Timings use `RUSTC_BOOTSTRAP=1 cargo test -p ferros-tests -- -Z unstable-options --format json
--report-time` (nextest is not installed).

## 1. Executive summary

**Overall health: good, with a weak middle.** The suite is large (407 tests, about 22,500 lines),
has a real harness with per-test deadlines, tag-based process cleanup and launch-stall detection,
and most stop assertions match exact variants, threads and addresses. All 407 tests passed in every
run of this review, with no leaked fixtures. The weaknesses are a set of assertions that accept a
broken result as one of their allowed outcomes, cleanup paths that live outside the harness, and a
few tests that pass by winning a scheduling race or sleeping a fixed time.

**Top 5 problems**

1. **Assertions that accept a wrong answer** (F1–F3, verified by mutation). Three tests passed with
   the code under test deliberately broken: a kill that fabricates a terminal stop, a session that
   drops cached symbols, and a breakpoint that stops counting hits after three. All three are fixed
   in this PR. More of the same pattern remains (F7–F12).
2. **Cleanup outside the harness** (F5). `lifecycle.rs`'s `Grandchild` spawns an untagged shell and
   fixture, so the harness deadline cannot find them, and its `Drop` sends `kill -9` to a numeric pid
   that its shell may already have reaped (pid-reuse hazard). Raw `MacOSBackend` launches in the same
   file are also untagged.
3. **Timeout abandons, not cancels** (F6). On a deadline the controller kills tagged processes but
   the worker thread keeps its `Session`, `Spawned` child handle and output tail threads. An unreaped
   child or still-polling reader can outlive the test.
4. **Race-dependent and sleep-dependent tests** (F13–F17). `simultaneous_breakpoints_on_two_threads`
   retries up to 20 launches to win a race; several birth-policy and code-write tests prove "did not
   run" with a 300–400 ms sleep; the PTY suite types after a fixed 500 ms sleep.
5. **Mocks prove Session policy, not backend behaviour** (F18–F19). `MockBackend` checks pending
   stops before validating a signal request, applies scripted effects all at once, and ignores page
   permissions. Several mock-only areas have no native counterpart.

**Biggest speed win: run `tests/exceptions.rs` in parallel** (fixed in this PR). Its 29 tests all
took one process-wide `Mutex`, but only two of them measure the process's IPC space. With a
read/write lock (those two exclusive, the rest shared) the binary went from **37.1 s to 10.0 s**,
and it was the longest binary, so the whole suite's summed binary time went from **92.6 s to
40.3 s**. The second fix, reaping a `Spawned` child before the tag sweep, removes a fixed **2.0 s**
from each of 29 `t.spawn` call sites. Together: full `cargo test -p ferros-tests` wall time with a
warm build **111 s → 41 s**.

## 2. Findings

Severity: critical / high / medium / low. "Fixed" means fixed in this PR.

| ID | Sev | Category | Location | Evidence | Recommended fix |
|---|---|---|---|---|---|
| F1 | high | correctness | `tests/exit_watcher.rs:51` | `assert_ne!(d.last_stop(), Some(&Killed { signal: 9 }))` accepts any other stop. `Session::kill` documents "no terminal stop is fabricated" (`ferros-core/src/session/lifecycle.rs:295`). **Mutation M1:** set `last_stop = Some(Exited { code: 0 })` in the `KillCleanup` branch → test **passed**. | **Fixed:** `assert_eq!(d.last_stop(), None)`. M1 now fails the test. |
| F2 | high | correctness | `tests/debuginfo.rs:733` | The #99 regression test promises cached symbols survive a vanished relative path, yet its match arm `Err(SymbolLoad { path, .. }) => assert_eq!(path, relative)` accepts losing them. **Mutation M2:** in `session/images.rs`, after `previous.swap_remove(index)`, drop `kept.symbols` when the path no longer exists → test **passed**. | **Fixed:** both lookups must return `Ok(Some(main))`. M2 now fails with "cached symbols were dropped". Owner check: the arm was added with the test in `5108b31`; if a reopen error is ever legitimate here, document why. |
| F3 | medium | correctness | `tests/source_breakpoints.rs:112` | `file_line_breakpoint_hits_on_every_iteration` launches 5 iterations and checks 3. **Mutation M3:** stop incrementing `hit_count` at 3 (`session/breakpoints.rs:576`) → test **passed**. | **Fixed:** checks all five hits, then a clean `Exited { code: 0 }`. M3 now fails. |
| F4 | high | performance | `tests/exceptions.rs:23` | One `Mutex` serialized all 29 tests; only `repeated_stops_do_not_grow_the_debuggers_ports` and `kept_exception_rights_do_not_grow_during_steps` read process-wide port counts. Serial run 37.11 s = parallel run 37.21 s. Fault injectors are per-backend (`ferros-macos/src/task.rs:409`), not global. | **Fixed:** `RwLock`; the two port-count tests take `exclusive()`, the rest `shared()`. 10/10 isolated runs and 2 full-suite runs green; binary 37.1 s → 10.0 s. |
| F5 | high | correctness / leaks | `tests/lifecycle.rs:751` (`Grandchild`), `:855` (`Drop`); raw backend launches at `:150`, `:604` | `Grandchild::spawn` uses `Command::new("/bin/sh")` with no harness tag, so a test that hits its deadline leaves shell + fixture + log file. `Drop` runs `kill -9 <pid>` even after `ran_to_completion` reaped the shell (which reaped the fixture), so the pid may belong to another process. | Spawn through `TestContext::tag()` and pass the tag in argv; drop the numeric `kill` once the shell has been reaped; track the log file for deadline cleanup. |
| F6 | high | correctness / leaks | `src/harness.rs:126–148`, `:198` | On timeout `kill_all()` kills by tag and removes capture dirs, but the abandoned worker still owns `Spawned::child` (unreaped), `Debuggee::child_cleanup` and two `OutputBuffer` tail threads polling every 10 ms. `kill_debuggee` adds up to 2 s per tag after the deadline. | Move child handles and reader stop flags into `Shared` so the controller can reap and stop them. Add a self-test: deadline with a live `Spawned`. |
| F7 | medium | performance | `src/harness.rs:496` | `Spawned::drop` called `kill_debuggee` (waits up to `KILL_SETTLE` = 2 s for pids to leave the table) before reaping its own child, which stays a zombie until reaped. Measured: 2.05 s per drop. 29 `t.spawn` sites. `Debuggee::drop` is 38 ms (the session reaps first). | **Fixed:** kill and `wait()` the owned child first, then sweep the tag. Measured 35 ms. |
| F8 | medium | flakiness | `tests/lifecycle.rs:1003` (`detach_reports_a_private_stop_it_never_saw`, #206) | `8ef57bd` replaced a fixed deadline with "fail if `--progress` output stalls for `RELEASE_STALL`", which fixes the reported false timeout (a released process that runs slowly under a scheduling clamp). A second ordering assumption remains: `hide_next_detach_stop` forces "not observed", so detach sends one SIGCONT; if the real stop lands after that SIGCONT the backend correctly reports "may stay stopped" and the test's exit assertion fails. **Unverified** under load. The test also spends ~8 s on 600 real 10 ms sleeps. | Synchronize on the real stop before the first SIGCONT; let the fixture exit on a test-controlled gate after release instead of 600 timed ticks. |
| F9 | medium | correctness | `tests/lifecycle.rs:1222` | Pre-stopped detach test loops `while process_state(..).contains('T')`; an empty state or zombie also ends the loop, so a killed target passes. Reviewer mutation (send `SIGKILL` instead of `SIGCONT` in `send_detach_sigcont`) predicted to survive; **not run**. | Require a fixture acknowledgement written after detach. |
| F10 | medium | correctness | `tests/lifecycle.rs:275` | Signal-replacement test (`with_signal(thread, SIGCONT)`) passes if both signals are dropped, because `loop_counter` has no SIGCONT handler. | Use a fixture with a SIGCONT observer; require exactly one acknowledgement. |
| F11 | medium | correctness | `tests/single_step/basic_step.rs:253` | The explicit-kill branch calls `d.kill()`, then `drop(d)`, then checks the pid is gone. `Debuggee::drop` sweeps by tag, so it hides a `kill` that returned `Ok` without killing. | Check `wait_until_gone(pid)` before dropping `d`. |
| F12 | medium | correctness | `tests/source_breakpoints.rs:475` | Shared-owner test disables both owners, then deletes both. A regression that restores the original instruction on the first disable passes. | After the first disable, assert the other owner is enabled and the raw trap bytes are still armed. |
| F13 | medium | flakiness | `tests/exceptions.rs:62` | `SIMULTANEOUS_TRIALS = 20` launches to catch both threads trapping before task suspend. Took 4.4 s under load, 0.07 s alone. A scheduler that keeps losing the race fails the test. **Frequency unverified.** | Gate the two workers and confirm both Mach requests are queued before publishing. |
| F14 | medium | correctness | `tests/single_step/birth_policy.rs:457`, `:517` | Attached detach/drop tests `sleep(BIRTH_QUIET)` then accept any stop. They pass if the worker was never born yet or already ran because birth interception failed. | Require `Interrupted`, identify the new thread, check entry PC and hold before detach/drop. |
| F15 | medium | correctness | `tests/single_step/new_thread_hold.rs:450` | Birth step-off proves the creator is frozen by sleeping 300 ms and checking stdout. An unfrozen creator may simply get no CPU. | Sample the creator's Mach suspend count while the hook holds. |
| F16 | medium | correctness | `tests/code_writes.rs:113` | Each round sleeps 2 ms and checks `ran <= THREADS`; zero progress passes, and a late exception from the previous round can satisfy the next same-PC check. | Require per-worker progress on original code before each plant; distinct sites or a rendezvous per round. |
| F17 | medium | flakiness | `tests/repl_pty.rs:132`, `:217`, `:647` | Types after a fixed 500 ms sleep instead of observing the foreground handoff; `wait_exit` returns without draining the PTY, so trailing output (including an error) can be missed. **Frequency unverified.** | Wait for the target's foreground process group; drain to EOF before final assertions. |
| F18 | medium | coverage (mock drift) | `ferros-core/src/backend/mock.rs:1012` vs `ferros-macos/src/task.rs:1722` | The mock checks pending stops before validating a signal request, and does not model held-signal records for suspended peers. Native code validates first. Mock tests of held/carried signals therefore check request construction only. | Keep the mock tests as Session-policy tests; add native negative cases (no held signal, out-of-range replacement, invalid request with a stop queued). |
| F19 | medium | coverage (mock drift) | `mock.rs:977`, `mock.rs:442` | Scripted stop batches apply every effect before the first event; resume refusal never exercises suspend/step-bit rollback (`task.rs:2165`). `read_bytes` ignores page protection. | Native refusal test with a `Step`/`Suspend` request checking suspend counts and step bit; native read across a readable→protected boundary. |
| F20 | medium | coverage | `tests/debuginfo.rs:244` | The poisoned-dSYM-cache test copies an *executable* into the cache, which fails on "no DWARF" before UUID validation is reached. | Poison with a foreign dSYM member of the same arch; assert its UUID differs. |
| F21 | medium | coverage | `tests/backtrace.rs:221` (#135) | The Rust symbol-table check only inspects legacy `_ZN` names and is vacuous if every std frame has DWARF. | Add one controlled stripped Rust frame and assert `SymbolSource::SymbolTable` plus the demangled name. |
| F22 | medium | coverage | `tests/symbols.rs:839` | Corrupt-unit test uses `addresses.iter().all(..)`, which accepts an empty or truncated set. | Compare exact results for known unaffected CUs. |
| F23 | medium | coverage | `tests/universal_images.rs:29` (#271) | No universal-file case with an unreadable UUID slice ahead of a valid one. | Synthetic fat file as #271 describes. |
| F24 | medium | coverage | `.github/workflows/rust.yml:3`, `:17` | Path filters listed `**.rs` but not the four C/asm sources `build.rs` compiles, so a fixture-only C change ran no CI. | **Fixed:** added `crates/ferros-tests/fixtures/**.c` and `**.s` to both triggers. |
| F25 | medium | flakiness | `scripts/cargo-test-runner.sh:56` | Check-then-`codesign --force` in place is unlocked; two concurrent cargo commands on one worktree can sign the same file while one execs it. The validation notes in this crate record a pre-test SIGKILL with an invalid signature. **Interleaving unverified.** | Sign into an immutable copy keyed by artifact hash and mode, or take a `flock`. |
| F26 | medium | coverage | `tests/platform_boundary.rs:128`, `:388` | Only scans `src/` trees (not `build.rs` or other target roots); the CLI branch returns after checking crate names, so a stray `cfg(target_os)` in another CLI file passes. | Scan every Cargo target root; enforce the single-`platform.rs` rule separately; add a regression per blind spot. |
| F27 | medium | coverage | `tests/lifecycle.rs:132` | Permission-denied launch tests fail at spawn, never at `task_for_pid` after a successful spawn, so the discard-child path is untested. `tagged_pids` skips zombies, so it cannot prove reaping. | Inject task acquisition failure after spawn and assert process-table disappearance. |
| F28 | low | correctness | `tests/stepping.rs:48`, `tests/non_local_exits.rs:137` | Step-stop checks `SingleStep { .. }`; the `siglongjmp` check builds its expected thread from the event itself. | Capture the expected thread before resuming and compare. |
| F29 | low | correctness | `tests/single_step/basic_step.rs:211` | Detach-after-step accepts a zombie of any exit status. | `Spawned::try_wait` and require `status.success()`. |
| F30 | low | correctness | `tests/fixtures.rs:67`, `:95`; `tests/cli.rs:148` | `contains("final count: 3")` also matches `30`; `contains("nested result:")` accepts any value; CLI `finish`/`next` check only a location. | Match whole lines and values. |
| F31 | low | correctness | `tests/backend_mock/line_breakpoints.rs:276` | Image-enumeration test has one frame and `let _ = session.symbolicate(..)`. | Two-image call chain; require `Some` for both lookups. |
| F32 | low | flakiness | `tests/backtrace.rs:1036` | The shared-cache test panics on the first unrelated `.symbols` file even when it already found `__pthread_once_handler`. **Unverified** on current hosts. | Run the on-disk probe only as a diagnostic after a name mismatch. |
| F33 | low | flakiness | `tests/backtrace.rs:454` | Requires std's private `std::thread::lifecycle::spawn_unchecked::` path. | Assert the fixture's own frames and the pthread base frames only. |
| F34 | low | design | `tests/macos_crate.rs:20`, `src/harness.rs:502`, `tests/lifecycle.rs:865` | Blocking `child.wait()` without a bound in cleanup paths; relies on the outer deadline. | Bounded poll with pid/status diagnostics. |
| F35 | low | design | `src/transcript.rs:190`, `:343`, `:511` | `wait_closed(..)` results discarded, so a CLI transcript can be partial if a descendant holds a pipe. CLI inherits `HOME`, writing logs to the developer's `~/.ferros`. | Require EOF by a shared deadline; per-test `HOME`. |
| F36 | low | design | `fixtures/c/soft_faults.c:60` | Reads `message.request.thread.name` after `mach_msg` reused the union for the reply. Fixture-local, exits right after. | Save the thread name before receiving. |
| F37 | low | design | `src/staging.rs:189` | Test-side Mach-O UUID and dSYM path logic via `object`. Not a core-boundary violation, but duplicates `ferros-macos` knowledge. | Reuse `ferros-macos` helpers where they are not the oracle under test. |
| F38 | low | performance | `crates/ferros-tests/Cargo.toml`, `tests/` | 28 integration-test binaries, each linking `ferros-core` + `ferros-macos`; touching `src/lib.rs` costs 13.1 s wall / 66 s CPU to rebuild (`ferros-core`: 6.7 s / 39 s). 1.2 GB under `target/debug/build/ferros-tests`. | Fold real-process files into one binary of modules, as `backend_mock/` and `single_step/` already are. Measure before and after. |

No test in the crate carries `#[ignore]`; the two ignored items in every run are `ignore` doctest
fences in `src/harness.rs:8` and `src/transcript.rs:17`. Platform boundary: no test or helper in
this crate puts OS or format code into `ferros-core`; `just check-boundary` passes.

## 3. Timing baseline

Full-suite numbers (`cargo test -p ferros-tests`, warm build, default test threads):

| Run | Wall incl. build check | Sum of binary times | Sum of test times |
|---|---|---|---|
| Baseline (trunk `37efda7`) | 111 s | 92.6 s | 534.9 s |
| This PR, run 1 | 66 s (first run after rebuild) | 46.2 s | 205.2 s |
| This PR, run 2 | 41 s | 40.3 s | 201.8 s |

Slowest binaries:

| Binary | Baseline | This PR | Cause / remaining cost |
|---|---|---|---|
| `exceptions.rs` | 37.2 s | 6.6 s | F4 (global mutex) and F7. Remaining: retry-budget tests that are timed by design (1.5–4 s each). |
| `lifecycle.rs` | 10.4 s | 10.0 s | Long pole `released_processes_run_to_completion` 8.7 s and `detach_reports_a_private_stop_it_never_saw` 8.2 s: `loop_counter 100`/`600` with real 10 ms sleeps (F8). |
| `single_step/` (`main.rs`) | 7.8 s | 7.7 s | `birth_policy::{detach,drop}_releases_a_held_new_thread…` 7.6 s each: `spawn_thread pause=4000` plus `BIRTH_QUIET` sleeps (F14). |
| `backtrace.rs` | 7.9 s | 0.7–6.8 s | `bt_prints_the_frames` builds the signed CLI on first use in the binary; varies with build cache state. |
| `signal_tracing.rs` | 4.4 s (15.6 s serial) | 0.4 s | F7: 8 `t.spawn` drops × 2 s. |
| `source_breakpoints.rs` | 2.6 s (10.9 s serial) | 0.5 s | F7. |
| `code_writes.rs` | 3.7 s | 3.6 s | `failed_overlapping_birth_write…` 3.6 s; birth pause. |
| `harness.rs` | 2.3 s | 2.3 s | Self-tests that wait out `KILL_SETTLE` on purpose. |
| `debuginfo.rs` | 2.1 s | 0.25 s | F7 (`spawn_thread pause=4000` attach). |

Slowest individual tests at baseline were all in `exceptions.rs` at 12–17 s: their own run time is
0.06–4 s (serial run), the rest was waiting for the file's mutex.

Expected further wins (not done here):

| Item | Expected gain |
|---|---|
| Gate-driven exit for `loop_counter` in the two release tests (F8) | `lifecycle.rs` 10 s → ~3 s (bounded by the next-slowest test, 2.2 s) |
| Replace `pause=4000` + sleeps with a gate in `birth_policy` (F14) | `single_step` 7.7 s → ~4.1 s |
| One real-process test binary (F38) | Link work for ~22 binaries collapses to one; incremental rebuild after a harness edit should drop well below 13 s. Not measured. |

After F8 and F14 the suite would be bound by the ~4 s exception retry tests, roughly 30 s of
summed binary time.

## 4. Fix plan

Each item is one small PR. "Verify" names the test that fails before and passes after, or a timing.

**Done in this PR**

- **P0a. Parallel exception tests (F4).** `tests/exceptions.rs`. Verify: `cargo test -p ferros-tests
  --test exceptions` 37.1 s → 10.0 s, 10/10 green.
- **P0b. Reap `Spawned` before the tag sweep (F7).** `src/harness.rs`. Verify: `Spawned` drop 2.05 s
  → 35 ms; `signal_tracing` serial 15.6 s → 0.4 s parallel.
- **P0c. Tighten three assertions (F1, F2, F3).** `tests/exit_watcher.rs`, `tests/debuginfo.rs`,
  `tests/source_breakpoints.rs`. Verify: mutations M1, M2, M3 (section 2) each fail the new test and
  passed the old one.
- **P0d. CI path filters (F24).** `.github/workflows/rust.yml`. Verify: a PR touching only
  `fixtures/c/*.c` triggers the Rust workflow.

**Next, in order**

1. **Tag `Grandchild` and raw backend launches (F5).** `tests/lifecycle.rs`. Verify: a self-test
   that times out with a `Grandchild` alive leaves no `ferros-fixture-` process. No dependencies.
2. **Controller owns reaping and reader shutdown on timeout (F6).** `src/harness.rs`,
   `tests/harness.rs`. Verify: new `deadline_reaps_a_spawned_child` self-test fails today (child
   left as zombie), passes after. Depends on P0b.
3. **Gate-driven release fixtures (F8).** `fixtures/loop_counter.rs` (or a new gate fixture),
   `tests/lifecycle.rs`. Verify: both release tests under 2 s; `detach_reports_a_private_stop…`
   30/30 alone and 5/5 in the full `lifecycle` binary (the #206 acceptance bar). Depends on 1.
4. **Prove holds instead of sleeping (F14, F15).** `tests/single_step/{birth_policy,new_thread_hold}.rs`.
   Verify: mutation "skip `sync_birth_policy` during selective resume" fails the test; binary time
   drops ~3.5 s.
5. **Assertion-tightening batch (F9–F12, F28–F31).** One PR per file. Verify: each finding's
   mutation fails the new test.
6. **Deterministic simultaneous-trap test (F13) and code-write rounds (F16).** `fixtures/threads.rs`,
   `fixtures/hot_loop.rs`, the two test files. Verify: single trial passes 50/50 under full-suite load.
7. **PTY synchronization (F17).** `tests/repl_pty.rs`. Verify: 50/50 under `stress` CPU load.
8. **Native counterparts for mock-only areas (F18, F19, F27).** `tests/signal_tracing.rs`,
   `tests/memory_reads.rs`, `tests/lifecycle.rs`. Verify: the reviewer mutations listed for F18/F19
   fail the new native tests.
9. **Debug-info oracles (F20–F23).** `tests/debuginfo.rs`, `tests/backtrace.rs`, `tests/symbols.rs`,
   `tests/universal_images.rs`. Verify: each finding's mutation fails.
10. **Runner signing race (F25).** `scripts/cargo-test-runner.sh`. Verify: two concurrent
    `cargo test -p ferros-tests --test smoke` loops for 5 min with no invalid-signature kill.
11. **Boundary checker blind spots (F26).** `tests/platform_boundary.rs`. Verify: synthetic violations
    in `build.rs` and a second CLI file are reported.
12. **One real-process test binary (F38).** Move `tests/*.rs` real-process files into
    `tests/native/` modules. Verify: incremental rebuild time after touching `src/lib.rs`, before and
    after. Do last: it conflicts with every item above.

Items 1–2 and 5–11 are independent of each other unless noted.

## 5. Open questions and unverified items

- **F2 intent.** Was the `SymbolLoad` arm in `relative_image_keeps_symbols_after_its_path_stops_resolving`
  deliberate (some platform path where reopening is expected)? The test's own doc says the symbols
  are kept, and the tightened test passes on trunk.
- **F4 safety margin.** Several exception tests assert wall-clock bounds (`took < 3 s`,
  `SHORT_RETRY_BUDGET + 2 s`). They held in 10 isolated parallel runs and 2 full-suite runs here, on
  16 cores. On a 3-core CI runner they now share CPU with each other; watch the first CI runs. If one
  flakes, give that test `exclusive()` rather than reverting.
- **#206.** The second ordering hazard (F8) is inferred from the code, not reproduced.
- **F13, F17, F25, F32** frequencies on CI hardware were not measured.
- Mutations predicted by reviewers for F9–F12, F14–F16, F18–F22 were run in #283; results are in the table below. F23 was out of scope for #283 and its mutation was **not run**. Only M1–M3 were run before.
### Mutation results (#283, base `a940af5`)

Each mutation was applied alone to a backed-up file, the named test was run once (three times where marked), and the file was restored. `git diff --stat -- crates/` was empty after every restore and at the end. Paths are under `crates/`. "Confirmed" means the old test **passed** on the mutated code, so the weakness is real. "Refuted" means it **failed**. Commands are `cargo test -p ferros-tests --test <file> <name>`.

| ID | Mutation | Test | Result | Note |
|---|---|---|---|---|
| F9 | `ferros-macos/src/task/ptrace.rs`, `send_detach_sigcont`: `libc::SIGCONT` → `libc::SIGKILL` | `--test lifecycle detach_resumes_a_target_stopped_before_attach` | **confirmed** (ok, 0.08 s) | Detach reaches this call on that path (`ptrace.rs` detach, after the private-stop wait). A killed target is a zombie or gone, not `T`, so the `contains('T')` loop ends. |
| F10 | `ferros-macos/src/task/ptrace.rs`, `select_resumed_signal`: added `let signal = if signal != original { 0 } else { signal };` so a replacement signal is dropped | `--test lifecycle held_signal_can_be_replaced_with_a_nonfatal_signal` | **confirmed** (ok) | Dropping both signals still ends in `Exited { code: 0 }` and the final count. |
| F11 | `ferros-macos/src/task/backend.rs`, `Backend::kill`: `if !shared.dead` → `if false && !shared.dead` (no SIGKILL); `reaped` → `Ok(())` without waiting; removed `self.exit_watcher = None` | `--test single_step kill_after_a_step_leaves_no_process` | **refuted** (FAILED at `basic_step.rs:256`, `pid survived (in flight: false)`) | With no SIGKILL and no reap, the pid is still present after `drop(d)` (harness also logs `cleanup left pid ... present`), so the pid check catches it. A first attempt that kept `self.exit_watcher = None` failed the same way. Whether the tag sweep in `Debuggee::drop` would hide a `kill` that fails in some other way was not tested. |
| F12 | `ferros-core/src/session/breakpoints.rs`, `release_sites`: removed `if enabled { continue; }` (the shared-owner guard), replaced with `let _ = enabled;` | `--test source_breakpoints shared_site_is_released_by_the_last_delete` | **confirmed** (ok, 0.09 s) | First disable restores the original instruction while the other owner is still enabled; the test never samples memory between the two disables. |
| F14 | `ferros-macos/src/task/thread_actions.rs`, `sync_birth_policy`: `if !holds_new_threads {` → `if !holds_new_threads \|\| true {` (birth traps never armed) | `--test single_step detach_releases_a_held_new_thread_and_the_process_finishes` and `drop_releases_a_held_new_thread_and_the_process_finishes` | **confirmed** (both ok, 7.6 s each) | Both tests accept any stop after `BIRTH_QUIET`; neither requires the newborn to be held. |
| F15 | `ferros-macos/src/task/thread_actions.rs`, `prepare_birth_step_off`: freeze loop guard `if suspend.contains(&port) \|\| leaving.contains(&port)` → `if true \|\| ...` (no peers frozen during step-off) | `--test single_step second_thread_created_during_birth_step_off_stays_at_the_trap` (3 runs) | **refuted** (3/3 FAILED at `new_thread_hold.rs:463`) | The unfrozen creator starts the second worker through the open entry within the 300 ms window on this host. The finding's concern, a creator that merely gets no CPU, is a flakiness risk the mutation does not show. |
| F16 | `ferros-macos/src/memory.rs`, `flush_caches`: early `return Ok(())` (no instruction-cache flush) | `--test code_writes planted_trap_is_executed_once_the_target_runs` (3 runs) | **refuted** (3/3 FAILED; e.g. `round 1: 23779 calls ran through the instruction replaced`) | The `ran <= THREADS` check catches stale-cache execution at once. The finding's other two gaps (zero progress; a late exception satisfying the next round) were not mutated. |
| F18 | `ferros-macos/src/task/state.rs`, `check_resume_request`: both signal validations disabled (`if false && ...` on the "requires a held soft-signal exception" and `!(0..32).contains(signal)` checks) | `--test backend_mock held_signals` (11 ok); `--test lifecycle signal` (14 ok); `--test single_step held_signal` (1 ok) | **confirmed** | No mock or native test depends on the native validation. The "stays suspended" check was left in place and not mutated. |
| F19 | `ferros-macos/src/task/backend.rs`, `Backend::resume`: removed `backend.undo_thread_actions();` from the `restore` closure (a refused resume keeps step bits and suspends) | `--test backend_mock` (89 ok); `--test single_step failed_birth_resume_can_still_continue`; `--test lifecycle early_reply_and_task_resume_failures_are_reported_once_each`; `--test lifecycle held_signal_survives_a_forwarding_resume_whose_task_resume_fails` | **confirmed** (all ok) | The native refusal tests inject `task_resume` failures but never inspect suspend counts or step bits. The read-across-protection half of F19 was not mutated. |
| F20 | `ferros-macos/src/macho/debuginfo.rs`, `valid_slice`: `file.mach_uuid().ok()? == Some(uuid)` → `file.mach_uuid().ok()?.is_some()` (UUID comparison removed) | `--test debuginfo default_cargo_build_uses_dsymutil_then_reuses_validated_cache` | **confirmed** (ok, 0.21 s) | The poisoned cache entry is an executable and is rejected by the DWARF check before the UUID is compared. |
| F21 | `ferros-core/src/symbols/demangle.rs`, `make_symbol_name`: no demangling when `source == SymbolSource::SymbolTable` | `--test backtrace frames_without_dwarf_below_main_are_named_from_the_symbol_table` | **confirmed** (ok, 0.09 s) | Without the mutation the `_ZN` demangle assertions only run for a `SymbolTable` frame whose raw name starts with `ZN`; the test passes with demangling off, so no such frame is exercised (the base frame `start` is not a Rust-mangled name). |
| F22 | `ferros-core/src/symbols/lines.rs`, `line_addresses`: added `addresses.truncate(1);` after `dedup()` (returns one address per line) | `--test symbols one_unreadable_unit_does_not_hide_the_others` | **confirmed** (ok, 0.01 s) | `.all(\|a\| expected.contains(a))` accepts a subset. `intact` is built by the same function, so it is truncated too. |

Summary: 9 confirmed (F9, F10, F12, F14, F18, F19, F20, F21, F22), 3 refuted by the mutation chosen (F11, F15, F16), 0 not mutable. Refuted here means the chosen mutation was caught; the finding's remaining gaps (see the notes) are untested.

Clean-code check after all restores: `just test-integration` exit 0 (30 `test result: ok`, 0 failed); `git diff --stat -- crates/` empty; no `ferros-fixture-` process from this worktree.

- `cargo nextest` was not available, so per-test times come from libtest `--report-time`, which
  includes time a test spends blocked on a lock.
- x86_64 (Rosetta) behaviour was out of reach: the crate's native tests and `build.rs` only target
  aarch64.
