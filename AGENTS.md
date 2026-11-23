# Agent and Contributor Guide

## What this is
Ferros is a Rust debugger designed for macOS arm64. The long-form narrative is [`docs/architecture.md`](docs/architecture.md). Architecture decisions that are still open are recorded under [`docs/adr/`](docs/adr/README.md).

`docs/architecture.md` states that ownership, lifetimes, and MIR are goals, not current behavior. Its crate map, `Session` / `Backend` split, image list, stop channel, and `BreakpointStore` description match the code.

## Module map
Cargo workspace members, one purpose each:

| Crate | Path | Role |
|---|---|---|
| `ferros` | `crates/ferros` | CLI. Names a platform crate only in `crates/ferros/src/platform.rs`. |
| `ferros-core` | `crates/ferros-core` | Portable engine. No OS API, no object-file format, no `cfg(target_os)`, no `unsafe`. |
| `ferros-macos` | `crates/ferros-macos` | macOS `Backend` and all Mach-O knowledge. The only crate allowed to use `unsafe`. |
| `ferros-tests` | `crates/ferros-tests` | Fixture binaries, the integration harness, and tests that debug real processes. |
| `ferros-utils` | `crates/ferros-utils` | Shared logging (`tracing`). |
| `ferros-mir` | `crates/ferros-mir` | Empty workspace member, reserved for a future compiler-IR crate. |
| `ferros-protocol` | `crates/ferros-protocol` | Empty workspace member, reserved for a future protocol crate. |
| `ferros-ui` | `crates/ferros-ui` | Parked TUI. Excluded from the workspace (`exclude` in the root `Cargo.toml`). It does not build against the current engine. |

There is no `ferros-linux` or `ferros-windows` crate.

## Key interfaces
Read these before changing engine behavior. One line each; the module docs carry the contracts.

| Interface | Path | What it is |
|---|---|---|
| `Backend` and its types | `crates/ferros-core/src/backend/` | Mechanism trait (`mod.rs`) plus `StopEvent`, `LaunchSpec`, `LoadedImage`, and `ResumeRequest` (`types.rs`). `mock.rs` is the in-memory backend. |
| `Session` | `crates/ferros-core/src/session/mod.rs` | Policy engine. Owns a `Box<dyn Backend>` and the process state machine. |
| `session::lifecycle` | `crates/ferros-core/src/session/lifecycle.rs` | `launch`, `attach`, `detach`, `resume`, `kill`, and `wait`. |
| `session::breakpoints` | `crates/ferros-core/src/session/breakpoints.rs` | Software-breakpoint table and logical breakpoints (file, function, or address). |
| `session::stepping` | `crates/ferros-core/src/session/stepping.rs` | Resumable, serial step-over of software breakpoints. |
| `session::line_stepping` | `crates/ferros-core/src/session/line_stepping.rs` | Source-line step over, into, and out. |
| `session::location` | `crates/ferros-core/src/session/location.rs` | Resolves a `BreakpointLocation` to sites in the main executable. |
| `session::stack` | `crates/ferros-core/src/session/stack.rs` | Stack traces and symbolication. The `MH_MAGIC_64` scan still here is a leak; ADR 0001 moves it to `ferros-macos` ([#193](https://github.com/JamalLyons/ferros/issues/193)). |
| `session::images` | `crates/ferros-core/src/session/images.rs` | Loads `Backend::images` into the symbol cache. `same_build` treats two missing UUIDs as the same image and two different UUIDs as a new one. |
| `session::exits` | `crates/ferros-core/src/session/exits.rs` | Non-local exits (panic landing pads and `longjmp`) so step-out sees a frame leave. |
| Stop channel | `Session::wait` → `StopEvent` in `crates/ferros-core/src/backend/types.rs` | The event a backend actually returns. |
| `DebuggerEvent` | `crates/ferros-core/src/events.rs` | Not published by any backend. Only the parked `ferros-ui` refers to it. |
| `MacOSBackend` | `crates/ferros-macos` (`src/task.rs`) | Mach task ports, exception messages, and `posix_spawn`. `Backend::images` is `src/images.rs` (the dyld image list, not only the main executable). |
| Test harness | `crates/ferros-tests` | `harness::run`, the `fixture!` macro, and `tests/`. See that crate's README. |

## Architecture decisions
[`docs/adr/README.md`](docs/adr/README.md) indexes the records. An accepted record is an owner decision and is restated in Conventions when it binds day-to-day work. A proposed record is not a rule. [ADR 0001](docs/adr/0001-object-file-boundary.md) is accepted.

## Toolchain
The project builds and tests with Rust/Cargo 1.100.0, pinned by `rust-toolchain.toml`, with the `clippy` component. Formatting uses `cargo +nightly fmt` for the unstable options in `rustfmt.toml`. The task runner for development and CI tasks is `just`.

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

Check the platform boundary, formatting and linting (clippy runs with warnings denied):
```sh
just fmt-check
just lint
just check-boundary
```

Before committing changes, ensure all five verification checks pass:
1. `just fmt-check`
2. `just lint`
3. `just test`
4. `just test-integration`
5. `just check-boundary`

## Rule
Every engine change lands with an integration test in `crates/ferros-tests` that fails without the change. Tests for known-broken behaviour are kept as `#[ignore = "..."]` with a comment naming the issue that will fix it, not deleted.

## Writing an integration test
Create a dedicated test file per area in `crates/ferros-tests/tests/`, gated with `#![cfg(target_os = "macos")]`. Run every test body inside `harness::run(|t| { ... })`. Resolve fixture binaries using the `fixture!("name")` macro, and look up breakpoint locations using source markers via `fixture.marker_line("<name>")`. Never hard-code line numbers or `target/...` paths. See `crates/ferros-tests/README.md` for the full example and API overview.

Exceptions, for tests that launch no fixture process:
- `tests/backend_mock.rs` drives the in-memory `MockBackend`, `tests/unwind_eh_frame.rs` unwinds over a synthetic image in the neutral `MockObjectFormat` and fake memory, and `tests/platform_boundary.rs` only reads source files; they use neither the macOS gate nor the harness.
- A test that launches nothing (for example attaching to a pid that has already exited, in `tests/macos_crate.rs`) may skip `harness::run`; it keeps the macOS gate and says why in a comment.

## Leaked processes
A suspended fixture keeps inherited pipes open and can hang the next command. After any integration run, verify that no orphaned fixture processes remain:
```sh
pgrep -fl ferros-fixture-
```
The command prints nothing and exits 1 when nothing matches; it also lists fixtures from other worktrees. If a run was interrupted, clean up only this worktree's fixtures:
```sh
just kill-fixtures
```
It prints each process it kills and exits 0 when none match. Do not use `pkill -f ferros-fixture-`: it also kills fixtures that agents in other worktrees are debugging and fails their runs.

## Parallel worktrees
Agents work in parallel git worktrees. [Worktrunk](https://worktrunk.dev) (`brew install worktrunk`, command `wt`) runs the project hooks in `.config/wt.toml`; the first run asks you to approve them.
- Create a worktree with `wt switch -c <branch>`. Its `pre-start` hook clones the primary worktree's `target/` (the only path in `.worktreeinclude`) as an APFS reflink: about 3 s, no extra disk, and the first build reuses compiled dependencies.
- A worktree made by another tool starts with an empty `target/`. Warm it once with `wt step copy-ignored` before the first build.
- `wt merge` runs the five checks from the Test section as its `pre-merge` hook, after rebasing. Run them alone with `wt hook pre-merge`.
- `wt remove` kills the worktree's leaked fixtures (`pre-remove`) before deleting it.

T3 Code creates its worktrees without `wt`, so these hooks do not run there. Each person adds two project scripts once in their own T3 app (top-right panel, **Add project script**). They are stored per machine, not in the repo, and scripts set through T3's API are ignored:

| Name | Command | Toggles |
|---|---|---|
| Warm build cache | `wt step copy-ignored --require-include` | Run automatically on worktree creation; Wait for it to finish before the agent starts |
| Kill fixtures | `just kill-fixtures` | Run in the thread's worktree when the thread settles |

## Troubleshooting
If a test fails with "task_for_pid was denied":
- If not signed: run through `just test-integration` so the cargo runner signs the executable automatically.
- If signed but denied: check `DevToolsSecurity -status` (Developer Mode must be enabled).
```sh
DevToolsSecurity -status
```
- If the detail says `Session::launch did not return within 5s`: macOS is waiting for a "Developer Tools Access" authorization. In a desktop session, approve the dialog once. On a machine with nobody at the screen (CI), the dialog cannot be answered; the `integration-macos` job in `.github/workflows/rust.yml` shows the machine setup step that removes it. The harness kills the half-launched fixture either way.

Two environment switches are available for diagnostic and harness verification:
- `FERROS_TEST_UNSIGNED=1`: the runner re-signs the test executable without the entitlement to verify the permission-failure path.
- `FERROS_TEST_FORCE_FAIL=1`: triggers an intentional harness test failure with a running fixture to verify process cleanup.

## Conventions
- Platform and format boundary: `crates/ferros-core` is fully platform-neutral and format-neutral. It has no OS dependency (`mach2`, `libproc`, `libc`), no `cfg(target_os)`, no `unsafe`, and no object-file format knowledge. All macOS code, including Mach-O (`MH_MAGIC` scanning, `LC_SYMTAB`, compact unwind, `LC_UUID`), lives only in `crates/ferros-macos`. A future `ferros-linux` (ELF) or `ferros-windows` (PE/COFF) crate keeps its own OS and format code; that code never enters `ferros-core` or the other platform crate. DWARF consumption stays in `ferros-core` behind a format-neutral image/object-file trait. `just check-boundary` and `crates/ferros-tests/tests/platform_boundary.rs` enforce the OS half (no OS crate, no `cfg(target_os)`, no `unsafe`). They do not yet reject Mach-O in core. The Mach-O still in `ferros-core` is a leak tracked by [#193](https://github.com/JamalLyons/ferros/issues/193) under accepted [ADR 0001](docs/adr/0001-object-file-boundary.md). Do not add more. The only `cfg(target_os)` switch outside the platform and test crates is `crates/ferros/src/platform.rs`.
- `unsafe`: Forbidden workspace-wide via `unsafe_code = "forbid"` in the root `Cargo.toml`. Only `crates/ferros-macos` permits `unsafe` for raw Mach FFI bindings; `crates/ferros-core` forbids it too.
- Formatting: Governed by `rustfmt.toml`. Format all code using:
```sh
just fmt
```
- Linting: `just lint` runs clippy with warnings denied. The workspace enables the `pedantic` and `cargo` lint groups; `crates/ferros-core` and `crates/ferros-macos` opt out of `pedantic` for now.
