# Agent and Contributor Guide

## What this is
Ferros is a Rust debugger designed for macOS arm64. The repository is organized as a Cargo workspace with the following crate layout: `crates/ferros` provides the command-line interface; `crates/ferros-core` implements the portable debugging engine and holds no OS-specific code; `crates/ferros-macos` implements it for macOS with Mach process control primitives; `crates/ferros-tests` contains fixture binaries and integration test harnesses; `crates/ferros-utils` provides shared utilities; `crates/ferros-mir` and `crates/ferros-protocol` are workspace members reserved for future compiler IR and protocol support (currently empty); and `crates/ferros-ui` is parked and excluded from the workspace.

## Toolchain
The project uses the nightly Rust toolchain pinned by `rust-toolchain.toml` with the `rustfmt` and `clippy` components. The task runner for development and CI tasks is `just`.

## Build
Compile all workspace crates:
```sh
just build
```

## Sign
On macOS, attaching to a process via `task_for_pid` requires either root privileges or the `com.apple.security.cs.debugger` code-signing entitlement. For arm64 macOS development, `.cargo/config.toml` configures `scripts/cargo-test-runner.sh` to automatically ad-hoc sign every executable invoked by Cargo (`cargo test`, `cargo run`, and doctests) with `crates/ferros-macos/ferros.entitlements`. Sudo is not needed and must not be used for tests.

To sign a manually executed binary directly:
```sh
just sign-debugger target/debug/ferros
```

Or with the raw `codesign` command (using `target/debug/ferros` as `<binary>`):
```sh
codesign --entitlements crates/ferros-macos/ferros.entitlements --force --sign - target/debug/ferros
```

## Test
Run unit tests and doctests (excludes `ferros-tests`, requiring no debugger entitlement):
```sh
just test
```

Run integration tests (launches real fixture processes using signed test runners):
```sh
just test-integration
```

Run tests for an individual crate with `just test-crate <crate>` (concrete example: `ferros-core`):
```sh
just test-crate ferros-core
```

Check formatting and linting (clippy runs with warnings denied):
```sh
just fmt-check
just lint
```

Before committing changes, ensure all four verification checks pass:
1. `just fmt-check`
2. `just lint`
3. `just test`
4. `just test-integration`

## Rule
Every engine change lands with an integration test in `crates/ferros-tests` that fails without the change. Tests for known-broken behaviour are kept as `#[ignore = "..."]` with a comment naming the issue that will fix it, not deleted.

## Writing an integration test
Create a dedicated test file per area in `crates/ferros-tests/tests/`, gated with `#![cfg(target_os = "macos")]`. Run every test body inside `harness::run(|t| { ... })`. Resolve fixture binaries using the `fixture!("name")` macro, and look up breakpoint locations using source markers via `fixture.marker_line("<name>")`. Never hard-code line numbers or `target/...` paths. See `crates/ferros-tests/README.md` for the full example and API overview.

Exceptions, for tests that launch no fixture process:
- `tests/backend_mock.rs` drives the in-memory `MockBackend`, and `tests/platform_boundary.rs` only reads source files; they use neither the macOS gate nor the harness.
- A test that launches nothing (for example attaching to a pid that has already exited, in `tests/macos_crate.rs`) may skip `harness::run`; it keeps the macOS gate and says why in a comment.

## Leaked processes
A suspended fixture keeps inherited pipes open and can hang the next command. After any integration run, verify that no orphaned fixture processes remain:
```sh
pgrep -fl ferros-fixture-
```
The command prints nothing and exits 1 when nothing matches. If a run was interrupted, clean up with:
```sh
pkill -9 -f ferros-fixture-
```
Note that `pkill` exits 1 when no matching processes are found.

## Troubleshooting
If a test fails with "task_for_pid was denied":
- If not signed: run through `just test-integration` so the cargo runner signs the executable automatically.
- If signed but denied: check `DevToolsSecurity -status` (Developer Mode must be enabled).
```sh
DevToolsSecurity -status
```
- If the detail says `Debugger::launch did not return within 5s`: macOS is waiting for a "Developer Tools Access" authorization. In a desktop session, approve the dialog once. On a machine with nobody at the screen (CI), the dialog cannot be answered; the `integration-macos` job in `.github/workflows/rust.yml` shows the machine setup step that removes it. The harness kills the half-launched fixture either way.

Two environment switches are available for diagnostic and harness verification:
- `FERROS_TEST_UNSIGNED=1`: the runner re-signs the test executable without the entitlement to verify the permission-failure path.
- `FERROS_TEST_FORCE_FAIL=1`: triggers an intentional harness test failure with a running fixture to verify process cleanup.

## Conventions
- Platform boundary: `crates/ferros-core` has no OS dependency (`mach2`, `libproc`, `libc`), no `cfg(target_os)` and no `unsafe`; `crates/ferros-tests/tests/platform_boundary.rs` enforces this. OS code goes in a platform crate (`crates/ferros-macos`). The only `cfg(target_os)` switch outside the platform and test crates is `crates/ferros/src/platform.rs`.
- `unsafe`: Forbidden workspace-wide via `unsafe_code = "forbid"` in the root `Cargo.toml`. Only `crates/ferros-macos` permits `unsafe` for raw Mach FFI bindings; `crates/ferros-core` forbids it too.
- Formatting: Governed by `rustfmt.toml`. Format all code using:
```sh
just fmt
```
- Linting: `just lint` runs clippy with warnings denied. The workspace enables the `pedantic` and `cargo` lint groups; `crates/ferros-core` and `crates/ferros-macos` opt out of `pedantic` for now.
