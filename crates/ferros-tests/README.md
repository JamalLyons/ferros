# ferros-tests

Integration test fixtures and test harness helpers for Ferros.

## Purpose

This crate provides standalone fixture binaries and helper utilities used by Ferros
integration tests to verify debugger functionality against real processes.

## Fixtures

The crate defines five fixture binaries:

- `ferros-fixture-hello`: Prints a greeting line and exits normally with status 0.
- `ferros-fixture-exit_code`: Exits immediately with non-zero exit status 7.
- `ferros-fixture-loop_counter`: Iterates a counter in a loop with small sleeps, printing the final count.
- `ferros-fixture-nested_calls`: Executes nested non-inlined function calls and prints the computed result.
- `ferros-fixture-threads`: Spawns two named worker threads synchronized with a barrier.

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
  with a comment naming the issue: `tests/breakpoints.rs` (continuing past a breakpoint,
  issue #33), and `tests/lifecycle.rs` / `tests/exceptions.rs` (a SIGKILL sent from outside
  while a stop is held, issue #56).
- `tests/lifecycle.rs` covers exit status and reaping, signals, interrupt/continue,
  timed waits, launch failures, ownership, drop, and death while stopped (issue #32).
  Process-gone assertions reject zombies.
- When the deadline passes, the harness kills that test's processes and fails the test with a
  "timed out" message. Permission failures fail within a few seconds with signing guidance.
- Use the `// BREAK: <name>` markers through `fixture.marker_line("<name>")` rather than
  hard-coding line numbers.
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
