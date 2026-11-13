# ferros-tests

Integration test fixtures and test harness helpers for Ferros.

## Purpose

This crate provides standalone fixture binaries and helper utilities used by Ferros
integration tests to verify debugger functionality against real processes.

## Fixtures

The crate defines nine fixture binaries:

- `ferros-fixture-hello`: Prints a greeting line and exits normally with status 0.
- `ferros-fixture-exit_code`: Exits immediately with non-zero exit status 7.
- `ferros-fixture-loop_counter`: Iterates a counter in a loop with small sleeps, printing the final count.
- `ferros-fixture-nested_calls`: Executes nested non-inlined function calls and prints the computed result.
- `ferros-fixture-signal_handler`: Installs a SIGCONT observer and reports when it runs (macOS).
- `ferros-fixture-signal_tracing`: Keeps two threads alive and acknowledges SIGUSR1 through
  files in a test-owned directory (macOS).
- `ferros-fixture-thread_signals`: Two workers raise thread-directed SIGUSR1 independently and report
  which worker handlers ran, for selective delivery and retained-stop races (macOS).
- `ferros-fixture-threads`: Spawns two named worker threads synchronized with a barrier. An optional
  first argument keeps the process running that many milliseconds after both workers exit; a second
  sets the worker sleep duration (default 50 milliseconds).
- `ferros-fixture-spawn_thread`: Starts one worker before a breakpoint location and another afterward,
  demonstrating the macOS resume suspension snapshot.

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
  leak check (`pgrep -f ferros-fixture-` must print nothing afterwards).

If a test fails with "task_for_pid was denied", the test executable is not signed with the
entitlement. Run through `just test-integration` (or sign it with the `codesign` command the
failure prints). If it is signed and still denied, check `DevToolsSecurity -status`.

## Writing an integration test

Put each area in its own file under `tests/`, gated with `#![cfg(target_os = "macos")]`, and run
every test body under `harness::run` (30 second deadline) or `harness::run_with_timeout`:

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
  stops (`tests/lifecycle.rs`) and breakpoint stops (`tests/exceptions.rs`) are enabled,
  including a signal deferred behind a breakpoint (issue #56). Client signal replies
  are answered early under task suspension; delivery decisions remain logical stops
  until resume requeues the selected signal on its original thread.
- `tests/signal_tracing.rs` covers selective per-thread signal delivery and a late exception published
  between resume lock sections, as well as private ptrace handshake races (#57): application signals
  arriving before or with the attach/detach SIGSTOP, and dropping a running backend or session
  whose signal stop is already pending. A one-shot diagnostic hook forces actual kernel signal
  requests into the two handshake orderings; the drop cases use naturally pending signals.
- `tests/lifecycle.rs` covers exit status and reaping, signals, interrupt/continue,
  timed waits, launch failures, ownership, drop, and death while stopped (issue #32).
  Process-gone assertions reject zombies.
- When the deadline passes, the harness kills that test's processes and fails the test with a
  "timed out" message. Permission failures fail within a few seconds with signing guidance.
- Use the `// BREAK: <name>` markers through `fixture.marker_line("<name>")` rather than
  hard-coding line numbers.
- `tests/single_step.rs` covers single-step and per-thread suspension in the backend (issue #33),
  driven through `d.session_mut().backend_mut()`. It finds fixture functions with
  `ferros_tests::symbols::function_address(&mut d, &fixture, "tick")` (any stopped `Session`), which reads the
  executable's Mach-O symbol table (no DWARF) and translates through the Session image mapping.
- `tests/breakpoints.rs` covers software breakpoints through `Session` (issue #33): hits and hit
  counts, stepping over a breakpoint on resume, `step_instruction`, consecutive and overlapping
  breakpoints, enable/disable/remove, threads, writes over a trap, and trap removal on detach and
  drop. `Session::read_memory` hides traps, so raw bytes are read through
  `session.backend_mut().read_memory`.
- `ferros_tests::arm64` decodes arm64 branches for tests that check where a step leaves the pc.
- `tests/exceptions.rs` covers the Mach exception plumbing (simultaneous stops, port rights, exception
  port restore on detach, failed replies). It reads Mach state through the safe
  `ferros_macos::diagnostics` module, and its tests run one at a time because they measure the
  test process's own IPC space.

## Tests without a real process

`tests/backend_mock.rs` is the one test file that launches no process and does not use the
harness. It drives `ferros_core::backend::mock::MockBackend`, an in-memory, scripted backend
enabled through the `test-util` feature, to prove the portable `Backend` trait and the mock work
from outside `ferros-core`. With nothing to launch it needs no entitlement, no fixtures and no
`#![cfg(target_os = "macos")]` gate.


## Debug-info regressions

`tests/debuginfo.rs` checks matching and mismatched dSYMs, a standalone plain-Cargo
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

- `tests/exit_watcher.rs` covers a competing debugger thread reaping a tracee before
  its exit watcher (#57): errors remain observable after death and across repeated
  waits, and kill teardown releases backend resources and clears Session ownership.

Stepping race coverage: `single_step.rs` races a real interrupt against a requested
instruction step and accepts either reported winner; scheduling does not guarantee that
both orderings occur. A separate bounded test requires a pending signal to beat a real
step, then checks that continuing produces no stray stop. The session
unit tests script an interrupt before internal step completion and separately force the
interrupt-policy flag when completion wins. They also script a simultaneous late
`SingleStep` after another event and verify it is discarded before continuing; that
precise simultaneous ordering remains mock-only. The macOS suspend-snapshot test also
shows that a thread created after resume can run despite the default `Suspend` action.

Draining events alongside an internal completion and carrying signals from an abandoned
step-over also retain deterministic session mock tests. Real kernel scheduling cannot
guarantee these exact message orderings; the mock tests force each ordering without
adding production hooks that synthesize kernel events. The real-process tests complement
these checks with actual interrupts, signal cancellation, and retained breakpoint replies.
For attached processes, the detach regression starts a step and covers both direct drop
(which stops and detaches internally) and explicit interrupt/wait/detach; public `detach`
continues to require a stopped session.
