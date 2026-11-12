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
        // Launched suspended at entry under a fresh debugger; stdout/stderr are captured.
        let mut d = t.launch(&fixture!("hello"));
        // `d` dereferences to `dyn ferros_core::Debugger`.
        d.resume().expect("resume");
        assert!(d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)));
    }); // `d` is dropped here: the process is killed, then the debugger is dropped.
}
```

- `t.launch_with_args(&fixture!("loop_counter"), &["100"])` passes fixture arguments; the harness
  appends a unique `--ferros-test-id=...` argument last.
- The returned `Debuggee` owns the debugger and kills the process when dropped, including when
  the test panics. Use `d.pid()`, `d.debugger()`, `d.stdout()` and `d.stderr()` as needed.
- When the deadline passes, the harness kills that test's processes and fails the test with a
  "timed out" message. Permission failures fail within a few seconds with signing guidance.
- Use the `// BREAK: <name>` markers through `fixture.marker_line("<name>")` rather than
  hard-coding line numbers.

## Tests without a real process

`tests/backend_mock.rs` is the one test file that launches no process and does not use the
harness. It drives `ferros_core::backend::mock::MockBackend`, an in-memory, scripted backend
enabled through the `test-util` feature, to prove the portable `Backend` trait and the mock work
from outside `ferros-core`. With nothing to launch it needs no entitlement, no fixtures and no
`#![cfg(target_os = "macos")]` gate.
