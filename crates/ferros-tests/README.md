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
