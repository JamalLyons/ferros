# Agent and Contributor Guide

## What this is
Ferros is a Rust debugger designed for macOS arm64. The long-form narrative is [`docs/architecture.md`](docs/architecture.md). Architecture decisions that are still open are recorded under [`docs/adr/`](docs/adr/README.md). Ownership, lifetimes, and MIR there are goals, not current behavior; its crate map, `Session`/`Backend` split, image list, stop channel, and `BreakpointStore` description match the code.

`docs/reviews/` is historical, not current guidance. Human setup (worktrees, T3 scripts, troubleshooting) is in [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Module map
Cargo workspace members under `crates/`, one purpose each:

- `ferros`: CLI. Names a platform crate only in `crates/ferros/src/platform.rs`.
- `ferros-core`: portable engine. No OS API, no object-file format, no OS `cfg`, no `unsafe`.
- `ferros-macos`: macOS `Backend` and all Mach-O knowledge. The only crate allowed to use `unsafe`.
- `ferros-tests`: fixture binaries, integration harness, tests that debug real processes.
- `ferros-utils`: shared logging (`tracing`).
- `ferros-mir`, `ferros-protocol`: empty members, reserved for a future compiler-IR crate and protocol crate.
- `ferros-ui`: parked TUI. Excluded from the workspace (root `Cargo.toml` `exclude`); does not build against the current engine.

There is no `ferros-linux` or `ferros-windows` crate.

## Key interfaces
Read before changing engine behavior; module docs carry the contracts. Paths under `crates/`.

| Interface | Path | What it is |
|---|---|---|
| `Backend` and its types | `ferros-core/src/backend/` | Mechanism trait (`mod.rs`); `StopEvent`, `LaunchSpec`, `LoadedImage`, `ResumeRequest` (`types.rs`). `mock.rs`: in-memory backend. |
| `Session` | `ferros-core/src/session/mod.rs` | Policy engine; owns a `Box<dyn Backend>` and the process state machine. |
| `session::lifecycle` | `ferros-core/src/session/lifecycle.rs` | `launch`, `attach`, `detach`, `resume`, `kill`, `wait`. |
| `session::breakpoints` | `ferros-core/src/session/breakpoints.rs` | Software-breakpoint table; logical breakpoints (file, function, address). |
| `session::stepping` | `ferros-core/src/session/stepping.rs` | Resumable, serial step-over of software breakpoints. |
| `session::line_stepping` | `ferros-core/src/session/line_stepping.rs` | Source-line step over, into, out. |
| `session::location` | `ferros-core/src/session/location.rs` | Resolves a `BreakpointLocation` to sites in the main executable. |
| `session::stack` | `ferros-core/src/session/stack.rs` | Stack traces and symbolication. Asks `ObjectFormat` for symbol tables and unlisted images. |
| `ObjectFormat` | `ferros-core/src/objfile/` | Format-neutral image trait (ADR 0001): mapped layout, build identity, symbol tables, DWARF/CFI bytes, unwind hints. `mock.rs`: `MockObjectFormat` (`test-util`). `ferros_macos::macho::MachOFormat` is the Mach-O implementor; `ferros_macos::new_session` pairs it with `MacOSBackend`. |
| `session::images` | `ferros-core/src/session/images.rs` | Loads `Backend::images` into the symbol cache. Reuses an image while `types::same_build` holds: two different known build ids are a new build, `BuildIdentity::Unknown` is no evidence, `Absent` never matches. |
| `session::exits` | `ferros-core/src/session/exits.rs` | Non-local exits (panic landing pads, `longjmp`) so step-out sees a frame leave. Images and entry symbols come from the session's `TargetRuntime`. |
| `TargetRuntime` | `ferros-core/src/runtime/` | Platform runtime knowledge: non-local exit sources, signal-trampoline saved state. Set with `Session::with_runtime`; without one nothing is trapped. `mock.rs`: `MockRuntime` (`test-util`). `ferros_macos::MacOSRuntime` implements it (libunwind, `__longjmp`, xnu `_sigtramp` frame). |
| Stop channel | `Session::wait` → `StopEvent` (`ferros-core/src/backend/types.rs`) | The event a backend actually returns. |
| `DebuggerEvent` | `ferros-core/src/events.rs` | Published by no backend. Only parked `ferros-ui` uses it. |
| `MacOSBackend` | `ferros-macos/src/task/` | Mach task ports, exception messages, `posix_spawn`. `mod.rs`: the struct and inherent API; `backend.rs`: the `Backend` impl and `Drop`; `handler.rs`, `thread_actions.rs`, `ptrace.rs`, `state.rs`: its helpers. `Backend::images` is `src/images.rs` (the dyld image list, not only the main executable). |
| Test harness | `ferros-tests` | `harness::run`, `fixture!`, `tests/`. See that crate's README. |

## Architecture decisions
[`docs/adr/README.md`](docs/adr/README.md) indexes the records. An accepted record is an owner decision, restated in Conventions when it binds day-to-day work; a proposed record is not a rule. [ADR 0001](docs/adr/0001-object-file-boundary.md) is accepted.

## Toolchain
Rust/Cargo 1.100.0 with `clippy`, pinned by `rust-toolchain.toml`. Formatting: `cargo +nightly fmt` (unstable options in `rustfmt.toml`). Task runner: `just`.

## Build
```sh
just build
```

## Sign
`task_for_pid` attach requires root or the `com.apple.security.cs.debugger` entitlement. `.cargo/config.toml` runs `scripts/cargo-test-runner.sh`, which ad-hoc signs every executable Cargo invokes (`cargo test`, `cargo run`, doctests) with `crates/ferros-macos/ferros.entitlements`. Sudo is not needed and must not be used for tests.

To sign a manually executed binary:
```sh
just sign-debugger target/debug/ferros
```
Raw: `codesign --entitlements crates/ferros-macos/ferros.entitlements --force --sign - <binary>`.

## Test
```sh
just test                     # unit tests and doctests; excludes ferros-tests, no entitlement needed
just test-integration         # launches real fixture processes with signed runners
just test-crate ferros-core   # one crate
just fmt-check
just lint                     # clippy, warnings denied
just check-boundary
```

Before committing, all five must pass: `just fmt-check`, `just lint`, `just test`, `just test-integration`, `just check-boundary`.

"task_for_pid was denied" or `Session::launch did not return within 5s`: see Troubleshooting in [`CONTRIBUTING.md`](CONTRIBUTING.md). Run integration tests via `just test-integration` so the runner signs them.

## Rule
Every engine change lands with an integration test in `crates/ferros-tests` that fails without the change. Known-broken behaviour keeps its test as `#[ignore = "..."]` with a comment naming the fixing issue; never delete it.

## Writing an integration test
Real-process tests are one binary, `crates/ferros-tests/tests/native/`: one module per area (`tests/native/<area>.rs`, declared in `tests/native/main.rs`), each gated with `#![cfg(target_os = "macos")]`. Run one area with `cargo test -p ferros-tests --test native <area>::`. All areas share one process, so a test that depends on process-wide state (its own IPC port counts, signal dispositions, environment variables, a global `tracing` subscriber) gets its own binary instead, as `tests/exceptions.rs` does. Run every test body inside `harness::run(|t| { ... })`. Resolve fixtures with `fixture!("name")` and breakpoint lines with `fixture.marker_line("<name>")`. Never hard-code line numbers or `target/...` paths. Full example and API: `crates/ferros-tests/README.md`.

Exceptions (tests that launch no fixture):
- `tests/backend_mock/` (in-memory `MockBackend`), `tests/unwind_eh_frame.rs` (synthetic image in `MockObjectFormat`, fake memory), and `tests/platform_boundary.rs` (reads source files and `cargo metadata`) use neither the macOS gate nor the harness.
- A test that launches nothing (for example attaching to a pid that has already exited, in `tests/native/macos_crate.rs`) may skip `harness::run`; it keeps the macOS gate and says why in a comment.

## Leaked processes
A suspended fixture keeps inherited pipes open and can hang the next command. After any integration run, verify no orphaned fixtures remain:
```sh
pgrep -fl ferros-fixture-
```
It prints nothing and exits 1 when none match; it also lists other worktrees' fixtures. If a run was interrupted, clean up only this worktree's fixtures:
```sh
just kill-fixtures
```
It prints each process it kills; exits 0 when none match. Do not use `pkill -f ferros-fixture-`: it also kills fixtures that agents in other worktrees are debugging and fails their runs.

## Parallel worktrees
Agents work in parallel git worktrees. A worktree not made by `wt switch -c` has an empty `target/`: run `wt step copy-ignored` once before the first build. Worktrunk and T3 setup: [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Skills
Shared agent skills come from the `vendor/agent-skills` submodule ([Agent-Skills](https://github.com/JamalLyons/Agent-Skills)); `.claude/skills/<name>` are relative symlinks into it.
After cloning, run `git submodule update --init`. Cargo and CI do not need it.
Update the pin with `git submodule update --remote vendor/agent-skills`, then commit the new pointer. A new upstream skill needs a matching `.claude/skills/<name>` symlink.

## Conventions
- Platform and format boundary: `crates/ferros-core` is platform-neutral and format-neutral: no OS dependency (`mach2`, `libproc`, `libc`), no OS `cfg` (`target_os`, `target_family`, `target_vendor`, `target_env`, a `unix`/`windows` predicate in `cfg`, `cfg!` or `cfg_attr`, or `std::os::unix` / `std::os::windows`), no `unsafe`, no object-file format knowledge. All macOS code, including Mach-O (`MH_MAGIC` scanning, `LC_SYMTAB`, compact unwind, `LC_UUID`), lives only in `crates/ferros-macos`. A future `ferros-linux` (ELF) or `ferros-windows` (PE/COFF) crate keeps its own OS and format code; it never enters `ferros-core` or the other platform crate. DWARF consumption stays in `ferros-core` behind a format-neutral trait. `ferros-core` sees images only through `objfile::ObjectFormat` and does not depend on the `object` crate. `just check-boundary` and `crates/ferros-tests/tests/platform_boundary.rs` enforce both halves per accepted [ADR 0001](docs/adr/0001-object-file-boundary.md): no OS crate, no OS `cfg`, no `unsafe`, no object-file parser dependency, no Mach-O text in core sources, comments included (`MH_MAGIC`, `LC_*`, `macho`/`Mach-O`, `nlist`, `__LINKEDIT`, `__unwind_info`, compact unwind, `dSYM`, and similar), and no macOS runtime text (`dylib`, `dyld`, `libunwind`, `libsystem_`, `unw_resume`, `__longjmp`, `xnu`, `_sigtramp`, `ucontext`, `mcontext`, `sigframe`, `uc_mcsize`): system libraries and signal frames come from `runtime::TargetRuntime`. The only `cfg(target_os)` switch outside the platform and test crates is `crates/ferros/src/platform.rs`.
- `unsafe`: forbidden workspace-wide via `unsafe_code = "forbid"` in the root `Cargo.toml`. Only `crates/ferros-macos` permits it, for raw Mach FFI bindings.
- Formatting: Governed by `rustfmt.toml`. Format all code with `just fmt`.
- Linting: `just lint` runs clippy with warnings denied. The workspace enables the `pedantic` and `cargo` lint groups; `crates/ferros-core` and `crates/ferros-macos` opt out of `pedantic` for now.
