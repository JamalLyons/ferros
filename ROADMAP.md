# Ferros Debugger Roadmap

_Last updated: 2026-11-26_

**Status:** Items are ticked once a test exercises them. Phase 1, epic [#26](https://github.com/JamalLyons/ferros/issues/26), closed on 2026-11-24 (81 of 82 sub-issues closed; the owner walkthrough [#40](https://github.com/JamalLyons/ferros/issues/40) is still open). Follow-ups [#182](https://github.com/JamalLyons/ferros/issues/182) and [#183](https://github.com/JamalLyons/ferros/issues/183) are closed. Phase 1.5, [#210](https://github.com/JamalLyons/ferros/issues/210), closed on 2026-11-26 (46 of 46 sub-issues closed). Phase 2, inspecting values (print, locals, frame, and the type layer), is next now that Phase 1.5 closed, per [#41](https://github.com/JamalLyons/ferros/issues/41). The owner walkthrough [#40](https://github.com/JamalLyons/ferros/issues/40) is unblocked and runs independently of Phase 2; it does not block Phase 2. Then Phase 3, the Debug Adapter Protocol, and Phase 4, async debugging, which depends on Phase 2. Hardware breakpoints, watchpoints, and x86-64 macOS are not done.

**Sequencing** ([#41](https://github.com/JamalLyons/ferros/issues/41), updated 2026-11-24):

1. Phase 1 ([#26](https://github.com/JamalLyons/ferros/issues/26)) — done. Closed 2026-11-24.
2. Phase 1.5 ([#210](https://github.com/JamalLyons/ferros/issues/210)) — done. Closed 2026-11-26.
3. Phase 2 — inspecting values (print, locals, frame, and the type layer). Next.
4. Phase 3 — editor integration (Debug Adapter Protocol).
5. Phase 4 — async debugging. Depends on Phase 2.

The owner walkthrough ([#40](https://github.com/JamalLyons/ferros/issues/40)) is unblocked. It runs independently of Phase 2 and does not gate it.

**Verified today by automated tests (`crates/ferros-tests`, plus the REPL tests in `crates/ferros`):**
- **Process control & lifecycle**: launching suspended, attaching by PID, task port acquisition, thread and memory-region enumeration, resuming, detaching, timed wait, process exit detection and reaping, signal pass-through/suppression, interrupt and kill (`tests/native/lifecycle/`, `tests/native/smoke.rs`).
- **Stop handling loop**: Mach exception port loop event pump surfaces asynchronous stops (`StopEvent` for breakpoints, signals, exits) to `Session::wait` without polling (`tests/exceptions.rs`, `tests/native/lifecycle/`).
- **Software breakpoints**: software breakpoints (`BRK`), hit counting, enable/disable/remove, trap-hiding on memory reads, trap cleanup on drop/detach, serial step-overs on resume (`tests/native/breakpoints.rs`), and source breakpoints by `file:line` and function name (`tests/native/source_breakpoints.rs`).
- **Single-stepping**: hardware single-step (`MDSCR_EL1.SS`) and per-thread stepping while keeping other threads suspended (`tests/native/single_step/`), plus source-level step over, into, and out (`tests/native/stepping.rs`).
- **Stack unwinding**: compact-unwind and DWARF CFI (`__eh_frame`, `.eh_frame_hdr`, `.debug_frame`), prologue/epilogue heuristics, signal and alternate-stack frames, inlined frames, and `FrameId`s (`tests/native/backtrace.rs`, `tests/unwind_eh_frame.rs`).
- **Symbols, DWARF, and images**: Mach-O and dSYM loading, demangled names, line tables, function lookup, symbol-table names, and loader-reported images including UUIDs and universal slices (`tests/native/symbols.rs`, `tests/native/debuginfo.rs`, `tests/native/images.rs`, `tests/native/universal_images.rs`).
- **REPL**: `ferros launch` and `attach` hold one session (interactive, piped, or `--batch`/`--command`) with run control, breakpoints, stepping, registers, memory, threads, backtrace, and images (`tests/native/cli.rs`, `tests/native/repl_pty.rs`, `crates/ferros/src/repl/tests.rs`).
- **Platform boundary**: strict isolation between portable engine (`ferros-core` has `#![forbid(unsafe_code)]` and zero OS dependencies) and platform backend (`ferros-macos`), verified by `scripts/check-boundary.sh` and `tests/platform_boundary.rs`.

This list captures concrete work needed to reach feature parity with production debuggers such as LLDB, GDB, Delve, and rr, with a focus on complete Rust language support. Items are grouped by subsystem to clarify ownership.

## ferros-core (debugger engine)

- [x] **Stop handling loop:** Introduce an event pump that listens for Mach exceptions/ptrace wait results and surfaces asynchronous stop reasons (signals, exits, breakpoints) to higher layers instead of the current polling-based `is_stopped`/`stop_reason` (verified on macOS by `tests/exceptions.rs` and `tests/native/lifecycle/`).
- [ ] **Register writes + context switching:** Per-thread general-purpose `write_registers` / `write_registers_for` is implemented and exercised (`tests/exceptions.rs` rewinds a real thread; `tests/backend_mock/`). Vector and floating-point registers are read and written by the arm64 macOS backend but have no test and are not shown by `registers`. The x86-64 register code is carried and unverified.
- [x] **Software breakpoints:** Add software breakpoints (BRK/INT3) with enable/disable/toggle semantics plus lifecycle tracking (requested, resolved, hit counts) and transparent trap hiding in memory reads (verified on macOS arm64 by `tests/native/breakpoints.rs`). Source breakpoints by `file:line` and function name are verified by `tests/native/source_breakpoints.rs`.
- [x] **Single-step execution:** Implement single-step execution and per-thread stepping while keeping other threads suspended, plus automatic step-over on resume (verified by `tests/native/single_step/` and `tests/native/breakpoints.rs`). Source-level step over (`next`), into (`step`), and out (`finish`) are verified by `tests/native/stepping.rs`.
- [ ] **Hardware breakpoints:** Implement hardware breakpoints using CPU debug registers (DR0-DR7 on x86-64, DBGBVR/DBGBCR on ARM64) for execution breakpoints that don't modify code. Helpers in `ferros-macos` program debug registers directly; they are not on `Backend` or `Session` and no test exercises them.
- [ ] **Watchpoints:** Implement data watchpoints (read/write/read-write) using CPU debug registers to break on memory access at specific addresses. The macOS helpers are unverified and not on `Session`.
- [x] **Stack unwinding & frame info:** Parse DWARF CFI (via Mach-O compact unwind, `__eh_frame`, `.eh_frame_hdr`, and `.debug_frame`) and prologue/epilogue fallbacks to build call stacks, detect inlined frames, and set a `FrameId` on each `StackFrame` (verified by `tests/native/backtrace.rs` and `tests/unwind_eh_frame.rs`; `backtrace` prints that stack).
- [x] **Symbol + DWARF ingestion:** Caching symbol layer: Mach-O and dSYM loading, demangling, line tables, function lookup, display names, and loaded-image identity (verified by `tests/native/symbols.rs`, `tests/native/debuginfo.rs`, `tests/native/images.rs`, `tests/native/universal_images.rs`, and `tests/native/backtrace.rs`). DWARF type extraction (`BinaryImage::describe_type`) has no tests and is not used by the session or the CLI. Resolving values of generics, enums, trait objects, and async state machines stays Phase 2, next, per [#41](https://github.com/JamalLyons/ferros/issues/41).
- [ ] **Memory APIs:** `Session` reads, writes, and region lists are tested (trap hiding in `tests/native/breakpoints.rs`; the REPL `memory` hexdump in `crates/ferros/src/repl/tests.rs`). The macOS backend caches pages on read and restores protection around code writes. Pattern search (`find_pattern`) is unused and untested, and there is no portable pattern-search API.
- [ ] **Async-aware runtime:** Understand async task schedulers (Tokio, async-std) by mapping generator frames back to async functions, surfacing pending tasks, and enabling “await-aware” stepping. Phase 4; it depends on Phase 2, per [#41](https://github.com/JamalLyons/ferros/issues/41).
- [ ] **Remote/recording support:** Define a `ferros-protocol` transport (TCP/Unix socket) plus optional record/replay support (rr-style) so sessions can be remote or deterministic.
- [ ] **Cross-platform parity:** Implement `LinuxBackend` (ptrace + /proc) and `WindowsBackend` (DbgHelp/DbgEng) implementing the `Backend` trait so we can debug on the same platforms that LLDB/GDB support by default.
- [ ] **Testing & safety:** Integration tests launch and attach fixture binaries on macOS arm64 (`crates/ferros-tests`: lifecycle, breakpoints, stepping, symbols, unwinding, the REPL). Linux and Windows backends, and fuzzing, are still planned. The scripted `MockBackend` covers the engine on any OS (`tests/backend_mock/`).

## ferros (CLI experience)

- [x] **Persistent session manager:** `ferros launch` and `ferros attach` open a line-oriented REPL (interactive, piped, or `--batch`/`--command`) whose commands operate on the current target (verified by `tests/native/cli.rs` and `tests/native/repl_pty.rs`). There is no separate debugger daemon.
- [ ] **LLDB/GDB-compatible commands:** Verified: `break`, `run`, `continue`, `step`, `next`, `finish`, `backtrace`, and `thread`, plus `stepi`, `registers`, `memory`, and `images` (`tests/native/cli.rs`, `tests/native/stepping.rs`, `tests/native/backtrace.rs`, `crates/ferros/src/repl/tests.rs`). Not done: `frame` (no frame selection), `watch`, and `info locals` / `info globals`. Frame selection and locals are Phase 2, next, per [#41](https://github.com/JamalLyons/ferros/issues/41).
- [ ] **Expression evaluation:** Provide `expr`/`print` commands backed by DWARF + MIR evaluation so users can inspect Rust values (Options, Results, enums, trait objects) similar to `lldb --rust-format`. Phase 2, next, per [#41](https://github.com/JamalLyons/ferros/issues/41).
- [ ] **Configuration & scripting:** Support `.ferrosrc`, alias definitions, macro commands, and scripting hooks (e.g., load Python/Lua scripts) to match LLDB/GDB automation capabilities.
- [ ] **Cargo integration:** Add `ferros run`/`test` wrappers that build with debug info, pick the right target triple, and pass binary paths automatically for a `cargo run -Zunstable-options` style flow.
- [ ] **Remote targets:** Allow `ferros connect <uri>` to talk to a remote `ferros-core` instance (or embedded target) via `ferros-protocol`, similar to `lldb-server`/`gdbserver`.
- [ ] **Session transcripts:** The interactive editor saves command history (`~/.ferros_history` or `$FERROS_HISTORY`; `tests/native/repl_pty.rs`). A transcript of command outcomes for sharing repro steps is not implemented. `--batch` replays a command list but does not record one.

## ferros-ui (TUI)

_`ferros-ui` is currently excluded from the workspace while it is rebuilt; `ferros launch`/`attach` open the line-oriented REPL (see `crates/ferros/README.md`)._

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
- [ ] **Documentation & tutorials:** A Phase 1 walkthrough is in `docs/walkthrough.md` (build a small program, break, step, backtrace). The owner walkthrough [#40](https://github.com/JamalLyons/ferros/issues/40) is unblocked now that Phase 1.5 ([#210](https://github.com/JamalLyons/ferros/issues/210)) closed on 2026-11-26. It runs independently of Phase 2 and does not gate it. There is no async-debugging guide.
- [ ] **CI & QA:** Linux CI builds the workspace and tests `ferros-core`. macOS CI runs format, clippy, unit tests, and the arm64 integration suite (`.github/workflows/rust.yml`). Attach/launch is not run on Linux or Windows, and there are no nightly stress tests.
- [ ] **Telemetry & crash reporting:** Capture anonymized crash dumps/hangs (opt-in) so stability matches production debuggers that ship with OS toolchains.
