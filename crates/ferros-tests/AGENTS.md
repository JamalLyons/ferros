# ferros-tests

The test contract is [`README.md`](README.md): harness API, fixtures, markers, one area per module. Root [`AGENTS.md`](../../AGENTS.md) Rule and "Writing an integration test" bind here.

Targets: `tests/native/` (one binary, macOS only), `tests/exceptions.rs` (own process), `tests/backend_mock/`, `tests/unwind_eh_frame.rs`, `tests/platform_boundary.rs`.

Off macOS only these run (everything else compiles empty and passes):
```sh
cargo test -p ferros-tests --test backend_mock --test unwind_eh_frame --test platform_boundary
```
