# Ferros Debugger Roadmap

_Last updated: 2026-11-13_

**Status:** Items are ticked once a test exercises them. The tracking epic is [#26](https://github.com/JamalLyons/ferros/issues/26).

**Verified today by automated tests (`crates/ferros-tests`):**
- **Process control & lifecycle**: launching suspended, attaching by PID, task port acquisition, thread and memory-region enumeration, resuming, detaching, timed wait, process exit detection and reaping, signal pass-through/suppression, interrupt and kill (`tests/lifecycle.rs`, `tests/smoke.rs`).
- **Stop handling loop**: Mach exception port loop event pump surfaces asynchronous stops (`StopEvent` for breakpoints, signals, exits) to `Session::wait` without polling (`tests/exceptions.rs`, `tests/lifecycle.rs`).
- **Software breakpoints**: software breakpoints (`BRK`), hit counting, enable/disable/remove, trap-hiding on memory reads, trap cleanup on drop/detach, and serial step-overs on resume (`tests/breakpoints.rs`).
- **Single-stepping**: hardware single-step (`MDSCR_EL1.SS`) and per-thread stepping while keeping other threads suspended (`tests/single_step.rs`).
- **Platform boundary**: strict isolation between portable engine (`ferros-core` has `#![forbid(unsafe_code)]` and zero OS dependencies) and platform backend (`ferros-macos`), verified by `scripts/check-boundary.sh` and `tests/platform_boundary.rs`.

This list captures concrete work needed to reach feature parity with production debuggers such as LLDB, GDB, Delve, and rr, with a focus on complete Rust language support. Items are grouped by subsystem to clarify ownership.

## ferros-core (debugger engine)

- [x] **Stop handling loop:** Introduce an event pump that listens for Mach exceptions/ptrace wait results and surfaces asynchronous stop reasons (signals, exits, breakpoints) to higher layers instead of the current polling-based `is_stopped`/`stop_reason` (verified on macOS by `tests/exceptions.rs` and `tests/lifecycle.rs`).
- [ ] **Register writes + context switching:** Finish `write_registers`, allow per-thread register contexts, and expose vector/floating-point registers to match LLDB’s `register write`/`register read --all`.
- [x] **Software breakpoints:** Add software breakpoints (BRK/INT3) with enable/disable/toggle semantics plus lifecycle tracking (requested, resolved, hit counts) and transparent trap hiding in memory reads (verified on macOS arm64 by `tests/breakpoints.rs`).
- [x] **Single-step execution:** Implement single-step execution and per-thread stepping while keeping other threads suspended, plus automatic step-over on resume (verified by `tests/single_step.rs` and `tests/breakpoints.rs`).
- [ ] **Hardware breakpoints:** Implement hardware breakpoints using CPU debug registers (DR0-DR7 on x86-64, DBGBVR/DBGBCR on ARM64) for execution breakpoints that don't modify code.
- [ ] **Watchpoints:** Implement data watchpoints (read/write/read-write) using CPU debug registers to break on memory access at specific addresses.
- [ ] **Stack unwinding & frame info:** Parse DWARF CFI and fallback heuristics to build accurate call stacks, detect inlined frames, and expose Frame IDs for CLI/UI consumption.
- [ ] **Symbol + DWARF ingestion:** Build a caching symbol/type layer (object file parsing, demangling, DWARF DIE navigation) so we can resolve Rust generics, enums, trait objects, async state machines, and display names as LLDB with `rust-analyzer` scripts does.
- [ ] **Memory APIs:** Add paged reads/writes, lazy caching, guard tracking, and utility helpers (hexdump, pattern search) similar to `memory read` in LLDB.
- [ ] **Async-aware runtime:** Understand async task schedulers (Tokio, async-std) by mapping generator frames back to async functions, surfacing pending tasks, and enabling “await-aware” stepping.
- [ ] **Remote/recording support:** Define a `ferros-protocol` transport (TCP/Unix socket) plus optional record/replay support (rr-style) so sessions can be remote or deterministic.
- [ ] **Cross-platform parity:** Implement `LinuxBackend` (ptrace + /proc) and `WindowsBackend` (DbgHelp/DbgEng) implementing the `Backend` trait so we can debug on the same platforms that LLDB/GDB support by default.
- [ ] **Testing & safety:** Add integration tests for attach/launch against fixture binaries across architectures (macOS arm64 verified in `crates/ferros-tests`; Linux/Windows and fuzzing planned).

## ferros (CLI experience)

- [ ] **Persistent session manager:** Introduce a long-lived debugger daemon or REPL so subcommands like `registers`, `memory`, `threads`, etc., operate on the current target instead of erroring out when invoked without a fresh attach.
- [ ] **LLDB/GDB-compatible commands:** Implement `break`, `run`, `continue`, `step`, `next`, `finish`, `backtrace`, `frame`, `thread`, `watch`, and `info locals/globals` so experienced users can transfer muscle memory.
- [ ] **Expression evaluation:** Provide `expr`/`print` commands backed by DWARF + MIR evaluation so users can inspect Rust values (Options, Results, enums, trait objects) similar to `lldb --rust-format`.
- [ ] **Configuration & scripting:** Support `.ferrosrc`, alias definitions, macro commands, and scripting hooks (e.g., load Python/Lua scripts) to match LLDB/GDB automation capabilities.
- [ ] **Cargo integration:** Add `ferros run`/`test` wrappers that build with debug info, pick the right target triple, and pass binary paths automatically for a `cargo run -Zunstable-options` style flow.
- [ ] **Remote targets:** Allow `ferros connect <uri>` to talk to a remote `ferros-core` instance (or embedded target) via `ferros-protocol`, similar to `lldb-server`/`gdbserver`.
- [ ] **Session transcripts:** Implement logging/recording of command history and outcomes for reproducibility and sharing repro steps.

## ferros-ui (TUI)

_`ferros-ui` is currently excluded from the workspace while the interactive front end is rebuilt; `ferros launch`/`attach` behave as `--headless`._

- [ ] **Command palette / mini-console:** Embed a small command line so users can run CLI commands without leaving the TUI (LLDB’s `command` window, Delve’s REPL).
- [ ] **Source + breakpoint view:** Render source/machine code next to the PC with breakpoint gutter, hit counts, and enable/disable toggles.
- [ ] **Call stack + frame locals:** Add panes for stack frames, local variables, arguments, captures, and watch expressions with Rust-aware formatting.
- [ ] **Thread/task explorer:** Display thread states, async tasks/futures, and allow activation/switching directly inside the TUI.
- [ ] **Rust value pretty-printing:** Plug into the planned DWARF/MIR layer to render enums/structs/options succinctly, matching LLDB’s Rust pretty-printers.
- [ ] **Breakpoint/watchpoint editors:** Provide dialogs or popovers to set addresses, symbolic names, conditional expressions, and logpoints.
- [ ] **Timeline/log panel:** Surface stop reasons, signals, breakpoint hits, and process output in a chronological log for post-mortem analysis.
- [ ] **Responsive layouts:** Offer presets for widescreen vs. laptop terminals and remember layout preferences.
- [ ] **Sync/async rendering considerations:** The TUI runs in an async context (`tokio::runtime`) but `ratatui` requires synchronous terminal drawing. Current implementation correctly uses `spawn_blocking` for event handling and output reading. **Watch out for:** (1) Debugger methods (`read_registers`, `threads`, `get_memory_regions`) called during rendering can block the async runtime—consider caching debugger state and updating it in `tick()` rather than during render; (2) Terminal restoration should happen immediately on quit (before cleanup) to prevent frozen appearance; (3) Use `tokio::time::sleep` instead of `std::thread::sleep` in async contexts; (4) Wrap blocking system calls (`kill`, `Command::output`) in `spawn_blocking` when called from async code.

## Cross-cutting initiatives

- [ ] **MIR integration:** Ingest MIR from `ferros-mir` to correlate machine state with high-level Rust constructs (drops, pattern matches, async state machines) and expose that context through CLI/UI.
- [ ] **Unified protocol:** Finalize `ferros-protocol` so CLI and TUI can both talk to headless debugger cores locally or remote, enabling IDE integrations later.
- [ ] **Documentation & tutorials:** Publish end-to-end guides (attach, stepping, async debugging) and troubleshooting docs comparable to LLDB/GDB manuals.
- [ ] **CI & QA:** Add cross-platform CI that runs attach/launch/regression suites on fixture programs, plus nightly stress tests (massive memory, many threads).
- [ ] **Telemetry & crash reporting:** Capture anonymized crash dumps/hangs (opt-in) so stability matches production debuggers that ship with OS toolchains.
