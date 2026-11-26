# ferros-tests

Integration test fixtures and test harness helpers for Ferros.

## Quickstart

Enough for the common case: one test that debugs a real fixture process. Read the reference
sections below only when this does not cover your test.

- Put the test in the area's module `tests/native/<area>.rs` (a new area is a new module, declared
  in `tests/native/main.rs`). Start each module with `#![cfg(target_os = "macos")]`.
- Run the whole body inside `harness::run(|t| { ... })` (30 s deadline). `t.launch*` returns a
  `Debuggee` that derefs to `ferros_core::Session`; dropping it kills and reaps the process.
- `fixture!("name")` resolves a fixture binary (names under [Fixtures](#fixtures)). Never write a
  `target/...` path.
- `fixture.marker_line("name")` gives the 1-based line of the `// BREAK: name` comment in the
  fixture source ([Marker Convention](#marker-convention)). Never write a line number.

```rust,ignore
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{BreakpointLocation, StopEvent};
use ferros_tests::{fixture, harness};

#[test]
fn stops_at_loop_call_then_exits()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &["3"]);

        let id = d
            .set_breakpoint(BreakpointLocation::Line {
                file: "loop_counter.rs".to_owned(),
                line: fixture.marker_line("loop_call"),
            })
            .expect("set_breakpoint");

        d.resume().expect("resume");
        let stop = d.wait(Some(Duration::from_secs(5))).expect("wait").expect("a stop");
        assert!(matches!(stop, StopEvent::Breakpoint { .. }), "got {stop:?}");
        let hit = d.breakpoint_hit().expect("a logical breakpoint hit");
        assert_eq!(hit.id, id);
        assert_eq!(hit.site.position.line, Some(fixture.marker_line("loop_call")));

        d.delete_breakpoint(id).expect("delete_breakpoint");
        d.resume().expect("resume");
        let end = d.wait(Some(Duration::from_secs(10))).expect("wait").expect("an exit");
        assert!(matches!(end, StopEvent::Exited { code: 0 }), "got {end:?}");
    });
}
```

Run one test (the cargo runner signs the executable; no sudo):

```sh
cargo test -p ferros-tests --test native <area>::<test_name>   # one test
cargo test -p ferros-tests --test native <area>::               # one area
just test-integration                                          # all of them
```

After any run, check for leaked fixtures. It prints nothing when clean (it also lists other
worktrees' fixtures). Clean only this worktree's with `just kill-fixtures`; never `pkill -f`.

```sh
pgrep -fl ferros-fixture-
just kill-fixtures
```

Contents of the reference sections: [Fixtures](#fixtures) · [Marker Convention](#marker-convention) ·
[Running the integration tests](#running-the-integration-tests) (signing, environment switches) ·
[Writing an integration test](#writing-an-integration-test) (launch variants, per-area test notes) ·
[Tests without a real process](#tests-without-a-real-process) ·
[Debug-info regressions](#debug-info-regressions)

## Purpose

This crate provides standalone fixture binaries and helper utilities used by Ferros
integration tests to verify debugger functionality against real processes.

## Fixtures

The crate defines twenty-two fixture binaries:

- `ferros-fixture-hello`: Prints a greeting line and exits normally with status 0.
- `ferros-fixture-exit_code`: Exits immediately with non-zero exit status 7.
- `ferros-fixture-loop_counter`: Iterates a counter in a loop with small sleeps, printing the final count.
- `ferros-fixture-nested_calls`: Executes nested non-inlined function calls and prints the computed result.
  Markers `call_a`, `call_b` and `call_c` sit on the call lines, for backtrace call-site checks.
  `leaf_mix` is a frameless leaf (compact unwind `MODE_FRAMELESS`, no `__eh_frame` FDE) called
  from `main` at marker `call_leaf`.
- `ferros-fixture-frameless_stack`: Before `main`, a dyld initializer `_frameless_caller` calls
  `_frameless_spill`, a leaf with a 32-byte stack allocation and an epilogue (compact unwind
  `MODE_FRAMELESS`, encoding `0x02002000`, no `__eh_frame` FDE). Both are assembly from
  `fixtures/asm/frameless_stack.s`, assembled and linked into this fixture only by `build.rs`
  (rustc gives such Rust leaves DWARF CFI in a debug build). Used for a backtrace stopped
  after the epilogue released the stack (issue #130). A second initializer
  `_unwind_only_caller` calls `_unwind_only_callee`, which saves the link register and
  frame pointer out of frame-record order (compact unwind `MODE_DWARF`: only its
  `__eh_frame` FDE describes it), then clears the frame pointer and points the link register
  at itself before a `nop`. Used for unwinding a copy stripped of its debug map and dSYM
  (`tests/native/debuginfo.rs`). Find them with `ferros_tests::symbols::raw_symbol_address`.
- `ferros-fixture-long_jump`: A C program (`fixtures/c/long_jump.c`, compiled and linked by
  `build.rs`; `fixtures/long_jump.rs` is a `#![no_main]` stub) whose markers live in the C
  source. Without arguments, `main` (marker `call_middle`) calls `middle` (marker
  `call_jumper`), whose callee `jumper` (marker `in_jumper`) `longjmp`s back to `main`
  (marker `after_jump`). With `--handler`, a SIGUSR1 handler (marker `in_handler`) leaves
  by `siglongjmp`, and its caller then calls `raise(0)` from the same frame (marker
  `raise_again`). Used for stepping across non-local exits (issue #141).
- `ferros-fixture-pthread_once`: A C program (`fixtures/c/pthread_once.c`, compiled and linked
  by `build.rs`; `fixtures/pthread_once.rs` is a `#![no_main]` stub) whose markers live in the
  C source. `main` (marker `call_once`) calls `pthread_once` on a static `pthread_once_t`. The
  callback `init_routine` (marker `in_init`) is entered from `__pthread_once_handler`, a static
  function of `libsystem_pthread` in the dyld shared cache. Used for a backtrace that names
  that function from the cache image's symbol table (issue #121).
- `ferros-fixture-dyld_aliases` opens a test library through absolute, relative,
  symlink, and hard-link paths. It reaches `_aliases_loaded` after every path resolves
  to the same symbol. The image test stops there and checks dyld's reported rows (#347).
- `ferros-fixture-caught_panic`: `inner` panics (marker `in_inner`) and `main`'s `catch_unwind`
  catches it; `middle` (marker `call_inner`) never sees the call return. `after_catch` is the
  first line after `catch_unwind`.
- `ferros-fixture-step_targets`: Callees whose demangled names do not parse as paths: a trait
  method on `Gate` with a bracket const-generic argument (markers `call_trait`, `in_trait`) and
  a closure called through `&dyn Fn` (markers `call_closure`, `in_closure`).
- `ferros-fixture-recursion`: `descend` recurses to the depth given by the first argument (default 60);
  marker `bottom` is reached at the deepest call. Used for deep backtraces.
- `ferros-fixture-signal_handler`: Installs a SIGCONT observer and reports when it runs (macOS).
  With `--step-resume`, raises SIGUSR1 at itself and stops after the handler (marker `after_signal`).
  With `--interrupt`, `main` (marker `call_spin`) calls `spin`, which loops forever on marker
  `spin_line` until a SIGUSR1 sent from outside runs the handler `on_sigusr1` (marker
  `in_handler`). Used for backtraces through `_sigtramp`.
- `ferros-fixture-signal_tracing`: Keeps two threads alive and acknowledges SIGUSR1 through
  files in a test-owned directory (macOS). `--own-group` moves it into its own process group so
  job-control stop signals reach it; `--catch-tstp` handles SIGTSTP and writes `tstp`.
- `ferros-fixture-thread_signals`: Two workers raise thread-directed SIGUSR1 independently and report
  which worker handlers ran, for selective delivery and retained-stop races (macOS).
- `ferros-fixture-step_races`: Two workers wait behind independent file gates before
  entering `step_site` (marker `step_site`). Each can raise its own SIGUSR1 afterwards.
  A fixture-side observer publishes the handler bitset on request; an exit gate releases
  both workers. With `--block-in-read`, worker 0 reads from an empty socket until the
  exit gate opens. Used to retain one exception while admitting the other worker, to observe
  carried-signal delivery, and to keep attached workers alive throughout teardown checks.
- `ferros-fixture-symbols`: Source-lookup targets read from its dSYM without a process: a generic
  function instantiated twice, an inherent method, a function in a module, an `#[inline(always)]`
  function inlined at two call sites (markers `call_inlined_first`, `call_inlined_second`), a
  comment-only line before marker `after_gap`, and two `twin` functions in two `twin.rs` files
  (`fixtures/symbols/left/`, `fixtures/symbols/right/`) for ambiguous name and file queries.
- `ferros-fixture-threads`: Spawns two named worker threads synchronized with a barrier. An optional
  first argument keeps the process running that many milliseconds after both workers exit; a second
  sets the worker sleep duration (default 50 milliseconds).
- `ferros-fixture-spawn_thread`: Starts one worker before a breakpoint location and another afterward.
  `pause=<milliseconds>` delays the second thread so a test can attach first. The second thread
  is a normal pthread and must stay stopped while the default resume action is `Suspend`.
- `ferros-fixture-hot_loop`: Threads (first argument, default 8, main included) that call `spin_site`
  in a tight loop with no sleep, keeping it hot in every core's instruction cache (`one-line`: main only, in a loop on one source line, marker `spin_line`). It prints the
  address of the counter `spin_site` increments (`calls=<address>`) before spinning.
- `ferros-fixture-workqueue`: Runs two GCD work items in turn, so the second normally reuses the
  first one's parked workqueue thread, which re-enters `_start_wqthread`. It prints whether it did (macOS).
- `ferros-fixture-soft_faults`: A C program (`fixtures/c/soft_faults.c`, compiled and linked
  by `build.rs`; `fixtures/soft_faults.rs` is a `#![no_main]` stub) whose markers live in the
  C source. `--null-store` stores through a null pointer (marker `null_store`).
  `--handler-once` raises SIGUSR1 once at a counting handler (marker `in_handler`) and
  prints `handler runs: <count>`. `--software-exception` sends its own thread's
  `EXC_SOFTWARE` exception with code `EXC_UNIX_ABORT` (not a Unix signal) to the task's
  exception handler and prints `exception reply: <code>` once answered. `--guard-page`
  maps a page of `0xa5` bytes followed by a `PROT_NONE` page, prints
  `boundary: 0x<protected page>` and sleeps. Used by `tests/exceptions.rs` (issue #244)
  and `tests/native/memory_reads.rs` (issue #290).
- `ferros-fixture-read_line`: Reads one line from stdin (marker `read_stdin`) and prints `read: <line>`,
  or `read: <eof>` at end of file. Used by `tests/native/cli.rs` to check which stdin `ferros launch` gives
  the program (issue #95).

## Marker Convention

Fixture source files include line markers formatted as trailing comments on executable statements:

```rust
let x = std::hint::black_box(42); // BREAK: marker_name
```

Each marker name is unique within a fixture file. Test helpers resolve marker names to 1-based source
line numbers to set breakpoints precisely without hardcoding line numbers in tests.

## Running the integration tests

```sh
just test-integration    # cargo test -p ferros-tests
```

The tests launch real fixture processes under the debugger, which needs `task_for_pid`. On arm64
macOS, `.cargo/config.toml` routes every executable cargo runs through
`scripts/cargo-test-runner.sh`, which ad-hoc signs it with the `com.apple.security.cs.debugger`
entitlement from `crates/ferros-macos/ferros.entitlements`. No sudo and no prompt are needed.
`just test` excludes this crate, so unit tests never depend on debugging permission.

Environment switches:

- `FERROS_TEST_UNSIGNED=1`: the runner re-signs the test executable without the entitlement, so
  the permission-failure path can be checked on purpose. The next normal run signs it again.
- `FERROS_TEST_FORCE_FAIL=1`: one harness test launches a fixture and then fails, for a manual
  leak check (`pgrep -fl ferros-fixture-` must print nothing afterwards).

If a test fails with "task_for_pid was denied", the test executable is not signed with the
entitlement. Run through `just test-integration` (or sign it with the `codesign` command the
failure prints). If it is signed and still denied, check `DevToolsSecurity -status`.

## Writing an integration test

Real-process tests live in one test binary, `tests/native/`: `main.rs` declares one module per
area (`tests/native/<area>.rs`, or a directory with `mod.rs` such as `single_step/`). Each module
starts with `#![cfg(target_os = "macos")]`. One binary means an engine change relinks one test
executable instead of one per area. libtest names a test `<area>::<test>`, so run one area with
`cargo test -p ferros-tests --test native <area>::`. All areas share one process: a test must not
depend on process-wide state (port counts, signal dispositions, environment variables, a global
`tracing` subscriber) that tests in other modules also touch. A test that does lives in its own
binary, as `tests/exceptions.rs` does. Run every test body under `harness::run` (30 second
deadline) or `harness::run_with_timeout`:

```rust,ignore
use std::time::Duration;

use ferros_tests::{fixture, harness};

#[test]
fn resume_runs_hello()
{
    harness::run(|t| {
        // Launched suspended at entry under a fresh `ferros_core::Session` (driving
        // `ferros_macos::MacOSBackend`); stdout/stderr are captured.
        let mut d = t.launch(&fixture!("hello"));
        // `d` dereferences to `ferros_core::Session`.
        d.resume().expect("resume");
        assert!(d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)));
    }); // `d` is dropped here: the session kills and reaps its target; the tag sweep is a backstop.
}
```

- `t.launch_with_args(&fixture!("loop_counter"), &["100"])` passes fixture arguments; the harness
  appends a unique `--ferros-test-id=...` argument last.
- `t.launch_with_backend(&fixture, &args, backend)` launches under a `MacOSBackend` the test built
  first, for example to keep a test hook handle (`backend.reply_fault_injector()`) after the session
  owns the backend.
- `t.spawn(&fixture!("loop_counter"), &args)` starts a fixture outside the debugger (stdio null, tagged
  and killed on the deadline like a launch), for tests that attach to a running process; dropping the
  returned `Spawned` kills and reaps it.
- The returned `Debuggee` owns the session and kills the process when dropped, including when
  the test panics. Use `d.pid()`, `d.session()`, `d.stdout()` and `d.stderr()` as needed.
- The fixture's stdout and stderr go to two files in a per-launch temporary directory (stdin is
  `/dev/null`); background threads tail them, and the directory is removed when the `Debuggee`
  is dropped.
- Tests for behaviour that is not implemented yet stay in the suite as `#[ignore = "..."]`
  with a comment naming the issue. The external SIGKILL regressions for held signal
  stops (`tests/native/lifecycle.rs`) and breakpoint stops (`tests/exceptions.rs`) are enabled,
  including a signal deferred behind a breakpoint (issue #56). Client signal replies
  are answered early under task suspension; delivery decisions remain logical stops
  until resume requeues the selected signal on its original thread.
- `tests/native/signal_tracing.rs` covers selective per-thread signal delivery and a late exception published
  between resume lock sections, as well as private ptrace handshake races (#57): application signals
  arriving before or with the attach/detach SIGSTOP, and dropping a running backend or session
  whose signal stop is already pending. A one-shot diagnostic hook forces actual kernel signal
  requests into the two handshake orderings; the drop cases use naturally pending signals.
- `tests/native/lifecycle.rs` covers exit status and reaping, signals, interrupt/continue,
  timed waits, launch failures, ownership, drop, and death while stopped (issue #32).
  Process-gone assertions reject zombies.
- When the deadline passes, the harness kills that test's processes and fails the test with a
  "timed out" message. Permission failures fail within a few seconds with signing guidance.
- Use the `// BREAK: <name>` markers through `fixture.marker_line("<name>")` rather than
  hard-coding line numbers.
- `fixture!("name")` returns a private copy of the built executable and its dSYM, taken once per
  test process under `$CARGO_TARGET_TMPDIR/ferros-fixtures/` (issue #84). Every Cargo command
  removes and recreates the uplifted `target/debug/ferros-fixture-*` files, so a test reading or
  launching that path while another Cargo command runs could find it missing. The copy keeps the
  executable's file name, and `fixture.path()` is the path the loader reports. Copies are keyed
  by Mach-O UUID and DWARF content (line-only rebuilds can keep the UUID), and removed by
  `cargo clean`. `tests/native/fixture_staging.rs` covers this.
- `tests/native/single_step/` covers single-step and per-thread suspension in the backend (issue #33),
  driven through `d.session_mut().backend_mut()`. It finds fixture functions with
  `ferros_tests::symbols::function_address(&mut d, &fixture, "tick")` (any stopped `Session`), which reads the
  executable's Mach-O symbol table (no DWARF) and translates through the Session image mapping.
- `tests/native/backtrace.rs` covers `Session::stack_trace` and the `backtrace`/`bt` command (issue #38):
  call-site lines from the `nested_calls` `call_*` markers, a stop on a function's first instruction,
  a spawned thread's trace ending at its base, a 60-level `recursion` trace under one second, and
  every frame pc in an executable region, an inlined frame in `symbols`, and a stop inside the
  frameless `leaf_mix`, and a stop on the `add sp` and the `ret` of the
  frameless `_frameless_spill` with a stack allocation (issue #130), and a stop in
  `pthread_once`'s `init_routine` that names the static `__pthread_once_handler` in the dyld
  shared cache from that image's symbol table (issue #121).
- `tests/native/code_writes.rs` checks that code the backend writes is what the target executes next
  (issue #80): a `brk` planted over an instruction the `hot_loop` threads have cached stops them.
- `tests/native/breakpoints.rs` covers software breakpoints through `Session` (issue #33): hits and hit
  counts, stepping over a breakpoint on resume, `step_instruction`, consecutive and overlapping
  breakpoints, enable/disable/remove, threads, writes over a trap, and trap removal on detach and
  drop. `Session::read_memory` hides traps, so raw bytes are read through
  `session.backend_mut().read_memory`.
- `tests/native/cli.rs` runs the real `ferros` binary (issue #36). `ferros_tests::transcript` builds it once
  per test process with `$CARGO build --package=ferros --message-format=json`, copies it next to
  itself and ad-hoc signs the copy with the debugger entitlement (set `FERROS_BIN` to use a signed
  binary instead). `transcript::launch_batch(&tag, &fixture, &args, &commands)` runs it with
  `--command`s and returns stdout, stderr and the exit status; `transcript::Interactive` drives it
  over pipes with `send(cmd)`, which returns the output up to the next `(ferros) ` prompt.
  `transcript::launch_batch_with` and `Interactive::launch_with_options` add `launch` options such
  as `--stdin <file>`; `launch_batch_with` can also write bytes to ferros's own stdin (the program
  inherits it in batch mode).
  `transcript::normalize` replaces addresses, pids and thread ids with placeholders. Reserve the
  identifying argument with `t.tag()`: the helpers pass it to the fixture after `--`, and the
  harness kills every process carrying it on the deadline and when the `Tag` is dropped.
- Scratch files a test creates: `t.scratch_file(stem, suffix)` (or `t.remove_on_cleanup(path)` for a path from
  `scratch::create_dir` / `unique_path`) registers the path with the harness, which removes it (file or
  directory tree) after killing the test's processes on normal completion, on a panic and when the deadline
  fires. A drop guard alone leaks on a deadline because the abandoned worker never unwinds.
- `ferros_tests::arm64` decodes arm64 branches for tests that check where a step leaves the pc.
- `tests/exceptions.rs` covers the Mach exception plumbing (simultaneous stops, port rights, exception
  port restore on detach, failed replies). It reads Mach state through the safe
  `ferros_macos::diagnostics` module, and its two tests that measure the test process's own IPC space
  take an exclusive lock over the file's other tests. It is a separate binary, not a `native`
  module, so tests of other areas cannot change those counts.

## Tests without a real process

`tests/backend_mock/` and `tests/unwind_eh_frame.rs` launch no process and do not use the
harness. `tests/unwind_eh_frame.rs` runs the stack unwinder over a synthetic `MockObjectFormat` image and fake
stack memory, pinning that `__eh_frame` FDEs are matched by runtime address. `tests/backend_mock/`
drives `ferros_core::backend::mock::MockBackend`, an in-memory, scripted backend
enabled through the `test-util` feature, to prove the portable `Backend` trait and the mock work
from outside `ferros-core`. With nothing to launch it needs no entitlement, no fixtures and no
`#![cfg(target_os = "macos")]` gate.

`MockBackend::stop_before_next_resume` (and `stop_before_nth_resume`, for a resume inside a
step-over) models a stop that arrives after the session's drain and before the backend checks
its queue: that resume is refused with `BackendError::StopPending`, as the macOS backend does.
The `*_reports_a_stop_that_arrives_after_its_drain` tests and
`step_over_reports_a_stop_that_arrives_before_its_final_continue` cover `resume`,
`step_instruction`, `step_out` and a step-over's final continue (#150); the native race behind
them is the one `lifecycle::replacement_signals_do_not_hide_later_signal_stops` hit under load.
A refused resume sent nothing, so it must lose no state: the next `ResumeRequest` still passes
the signals it held (`signal_of_a_refused_resume_is_passed_by_the_next_resume_with_signal`,
`refused_step_out_keeps_a_signal_carried_for_another_thread`,
`failed_step_instruction_keeps_the_threads_carried_signal`,
`refused_step_over_final_continue_keeps_another_threads_signal`), and a stopped handler's
rendezvous survives a refused step over its breakpoint
(`refused_step_over_keeps_a_stopped_handlers_rendezvous`) or over another one
(`unstarted_step_over_elsewhere_keeps_a_stopped_handlers_rendezvous`). When the session
consumes a handler's return in place of a deferred `resume_with_signal`, its continue passes
the signals that resume still held and drops them from those carried
(`handler_return_continue_passes_a_signal_carried_from_a_deferred_resume`).

Source steps drop stops that belong to no request where they drain or are refused (#148):
a breakpoint stop of a private trap that is gone, of any thread, queued for `start_line_step`
(`step_out_drops_another_threads_stale_trap_stop_queued_before_it`) or for the continue to
the return trap (`step_out_drops_a_stale_trap_stop_queued_for_its_return_continue`) or
refusing the continue from a deeper recursive frame
(`step_out_drops_a_stale_trap_stop_that_refuses_its_recursive_continue`), and an abandoned
step's late `SingleStep` refusing the step's first continue
(`step_out_drops_an_abandoned_steps_completion_that_refuses_its_continue`). A step that starts
at a user breakpoint drops one refusing the continue after the step off it
(`step_out_from_a_breakpoint_drops_a_stale_trap_stop_that_refuses_its_continue`). A queued
stop at an armed breakpoint is still reported
(`step_out_reports_a_real_breakpoint_stop_queued_for_its_return_continue`), and so is one at a
trap the program itself contains (an arm64 `brk #1`, not in the breakpoint table), queued
for `start_line_step` (`step_out_reports_another_threads_program_trap_queued_before_it`) or
for the continue to the return trap
(`step_out_reports_a_program_trap_stop_queued_for_its_return_continue`): only a stop with no
trap instruction of any form left at its address is stale.

Step-overs of breakpoints at the pc drop such a stop too (#158): one reported with the
internal step off a breakpoint, which the drain before the final continue drops
(`step_over_drops_a_stale_trap_stop_reported_with_its_internal_step`), and one refusing the
next step of a step-over with several sites, which is issued again
(`step_over_drops_a_stale_trap_stop_that_refuses_its_next_step`). The same stop at an armed
breakpoint or at a program `brk #1` is still reported at either point
(`step_over_reports_a_real_trap_stop_reported_with_its_internal_step`,
`step_over_reports_a_real_trap_stop_that_refuses_its_next_step`).

So do the internal resumes of a stopped signal handler's return (#165): a stale stop reported
with the handler's return to its rendezvous, or with a hit of its exit trap (a neutral
`MockRuntime` exit source standing in for `__longjmp`), is
dropped and the continue or exit followed on
(`handler_return_drops_a_stale_trap_stop_reported_with_it`,
`handler_exit_drops_a_stale_trap_stop_reported_with_it`); one refusing the reissued continue
or the exit's step is dropped and that resume issued again
(`handler_continue_drops_a_stale_trap_stop_that_refuses_it`,
`handler_exit_step_drops_a_stale_trap_stop_that_refuses_it`). A real trap's stop is still
reported at each point (the matching `*_reports_a_real_trap_stop_*` tests).

`MockBackend::die_refusing_nth_resume` models a target that dies right after a stop refuses
one of its resumes: memory writes then fail, and the terminal event comes after a delay (#155).
The cleanup after the refusal cannot put the traps back, and the death is still what `wait`
reports, for a step over the breakpoint at the pc
(`refused_step_over_on_a_dying_target_reports_its_death`) and for a source step's continue to
its return trap (`refused_step_out_continue_on_a_dying_target_reports_its_death`). A refused
internal resume inside `wait(Some(Duration::ZERO))` keeps to that budget: the session stays
running and the next `wait` reports the death
(`zero_timeout_wait_does_not_block_on_a_dying_targets_refused_continue`).


## Debug-info regressions

`tests/native/debuginfo.rs` checks matching and mismatched dSYMs, a standalone plain-Cargo
build with the default unpacked profile, validated cache reuse, tool failures and
timeouts, Session diagnostics and image refresh, and two distinct ASLR launches.
The Cargo-build test uses an isolated temporary package and keeps its object files
until `dsymutil` finishes. It launches no debuggee. Real launches use the normal
harness and verify both the main function and the reported library header addresses.

Lifecycle hardening (#58) covers detach's documented SIGCONT side effect, including a
previously job-control-stopped target; restore errors after teardown; independent reply
and task-resume failures; and reaping ownership across exit, kill, drop and detach.
The harness uses a shared `ChildCleanup` handle: only a detached, unreaped child grants
permission to wait. The real failed Mach-send cleanup is tested with isolated ports in
`ferros-macos`, without launching a fixture.

- `tests/native/exit_watcher.rs` covers a competing debugger thread reaping a tracee before
  its exit watcher (#57): errors remain observable after death and across repeated
  waits, and kill teardown releases backend resources and clears Session ownership.

Stepping race coverage (#71) uses controlled native orderings, with real macOS process
control and real kernel exception messages:

- `exceptions::kept_exception_rights_do_not_grow_during_steps` opens the first worker's
  gate, retains its breakpoint record with `Suspend`, then opens the second worker's
  gate. It checks that the first worker keeps an extra send-right reference during
  repeated step-overs of the second, while total names and send references stay stable.
  It never needs simultaneous traps or another launch.
- `single_step::breakpoint_cancels_a_real_step_without_a_stray_step_stop` steps a planted
  `brk`: the synchronous breakpoint precedes instruction completion. Immediately after
  `wait`, before any resume, MDSCR_EL1.SS is clear and every per-thread suspend count is
  zero. A plain continue produces no stray step.
- `single_step::signal_cancels_a_real_step_without_a_stray_step_stop` (#75) stops at
  the `svc` of a blocking read on arm64. With the socket empty and its writer gated,
  the test waits until Mach reports the stepping thread waiting, with MDSCR_EL1.SS set
  and its peers suspended. A thread-directed SIGUSR1 produces a real signal event
  before the step completes. The test checks the bit and suspend counts at event
  delivery, then releases the writer and requires a clean exit after plain continue.
- `single_step::interrupt_precedes_a_real_instruction_step` pauses publication of a real
  step exception, queues an interrupt first, and requires `Interrupted`. It checks the
  same immediate cleanup, then releases and observes the late real completion below
  Session before continuing without a stray step.
- `single_step::late_real_internal_step_is_dropped_after_abandonment` pauses publication
  of the internal step-over's real completion until the one-second Session drive times
  out. An interrupt abandons the plan; after release, publication is acknowledged and
  Session must silently consume the late `SingleStep`, without any intervening resume.
- `single_step::event_batched_with_real_internal_step_is_reported_before_resume` uses a
  narrowly armed hook: once a real step exception arrives, it queues SIGUSR1 to a peer,
  temporarily balances that peer's one thread suspension, receives its real signal
  exception, then restores the suspension. Both kernel events are published together.
  Session must report the signal, leave the stepping pc at the decoded successor of the
  marker instruction, and keep every thread stopped. The hook synthesizes no `StopEvent`
  and is inactive when unarmed.
- `single_step::retained_signal_survives_abandoned_real_step_over_or_is_explicitly_dropped`
  retains worker 1's real thread-directed SIGUSR1 while worker 0 is at a breakpoint.
  Publication of worker 0's internal completion is paused and an interrupt abandons
  that step-over. The next `resume_with_signal` runs worker 1's handler (bitset 2).
  Plain `resume` suppresses it (bitset 0), and a subsequent `resume_with_signal` proves
  the decision was discarded permanently. Both branches acknowledge worker progress
  before requesting the handler snapshot.
- `single_step::detach_cleanup_after_starting_step_releases_suspended_threads` directly
  observes peer suspend counts of one while a real completion is publication-paused,
  then covers both attached Session drop and explicit interrupt/wait/detach, requiring
  a successful child exit. `single_step::backend_drop_releases_directly_observed_step_suspends`
  (#75) drops an attached backend without calling `wait`. A one-shot hook samples a
  known peer's Mach suspend count immediately before detach sends its real private
  SIGSTOP. The count must already be zero there, and the child must exit normally.

- `single_step::handler_return_reissues_a_real_peer_breakpoint_step` (#176) asks the
  backend for thread lists sorted by port name (`ReplyFaultInjector::sort_thread_lists`),
  so the peer Session steps first does not depend on `task_threads` order (#183). It
  sets the publication hook's deadline to the test's stop timeout.
- `single_step::publication_guard_disarms_an_armed_hook` (#183) drops the
  `ReleasePublication` guard while the hook is armed but not yet paused; the next real
  step must be published without a pause.

The four Session mock tests for late abandoned steps, pending events alongside step
completion, and delivery/drop of carried signals remain as fast, isolated complements;
these orderings now also have native coverage above. The completion-winning interrupt
policy branch (`interrupt_requested` with a completed internal step), and scripted
backend failure paths remain unit-test coverage rather than claims about the native
scenarios above. The separate, non-stepping
`simultaneous_breakpoints_on_two_threads_are_both_reported` test still uses bounded
launch attempts; it is not used as setup or evidence for the retained-rights test.

`new_threads_can_run_after_suspend_snapshot`
is the original regression for that snapshot: a thread created after resume now stays
stopped until a later continue, including across a single-step of its pthread entry.
Other threads stay suspended while that entry instruction is restored. A launch that
is still inside dyld holds a later pthread the same way, by stopping at the
executable entry once the library is mapped.
An engine breakpoint on the entry is reported and the newborn stays suspended, and the
backend keeps its own port right to that newborn. Session breakpoints on `_thread_start`
and on the executable entry survive the backend's traps there, and `read_memory` shows the
original instruction under a trap. A step from the launch stop followed by a continue
holds nothing. With the `workqueue` fixture, a reused workqueue thread told to `Continue`
runs its second work item instead of being held.
