# ferros-core map

Portable engine. Root [`AGENTS.md`](../../AGENTS.md) Conventions bind here (platform and format boundary, no `unsafe`); this file only says where things are. Module docs carry the contracts.

Open first: `src/session/mod.rs` (state-machine table), then `src/backend/mod.rs` (the `Backend` contract).

## Modules (`src/`)
- `session/`: `Session`, the policy engine. `lifecycle`, `breakpoints`, `stepping`, `line_stepping`, `location`, `stack`, `images`, `exits`: one row each in the root key-interfaces table.
  - `stack.rs`: stack traces and symbolication. `Session::stack_trace_with_registers` returns each frame's `FrameRegisters` and DWARF lookup address from the same unwind.
- `values/`: `scope.rs` discovers variables in a logical frame. `location.rs` evaluates DWARF locations using frame registers and target memory. `types.rs` resolves types lazily by DIE offset or qualified Rust name in the per-image `TypeTable` exposed by `BinaryImage::types`. `format.rs` renders values with depth and element limits, and `pretty/` registers type-specific printers. `path.rs` parses `print <path>` (fields, `.0`, `[n]`, `*`, parentheses) and evaluates it to a typed place. In `session/variables.rs`, `Session::frame_variables` and `Session::selected_frame_variables` read arguments and locals, `Session::format_variable` supplies formatted text to `locals` and `print`, and `Session::format_path` renders the value a path selects.
- `session/tests.rs` + `session/tests/<area>.rs`: `Session` unit tests on `MockBackend`. Shared helpers live in `tests.rs`.
- `backend/`: `Backend` trait (`mod.rs`), `StopEvent`/`LaunchSpec`/`ResumeRequest` (`types.rs`), `BackendError` (`error.rs`), `MockBackend` (`mock.rs`, `test-util`).
- `objfile/`: `ObjectFormat`, the format-neutral image trait (ADR 0001). `contents.rs`: what a format reads. `mock.rs`: `MockObjectFormat`.
- `runtime/`: `TargetRuntime` (non-local exits, signal-frame state, system mutex lock state). `mock.rs`: `MockRuntime`.
- `symbols/`: DWARF and symbol work.
  - `image.rs`: `BinaryImage`, sections and segment mapping read through `ObjectFormat`.
  - `cache.rs`: per-session symbol cache, address symbolication.
  - `lines.rs`: line tables, `file:line` to addresses and back.
  - `functions.rs`: function name to entry and post-prologue address.
  - `unwind.rs`: stack walk (CFI, unwind hints, frame-pointer chain).
  - `demangle.rs`: Rust/C++ demangling. `load_inputs.rs`: retry stamps for failed loads.
- `arch/`: instruction-set knowledge. `arm64.rs`, `arm64_frame.rs` (prologue/epilogue analysis for `unwind.rs`), `x86_64.rs` (placeholder).
- `breakpoints/`: breakpoint and watchpoint bookkeeping; `builder.rs`.
- `types/`: `Address`, build identity, process/thread, registers, stack frames, symbols.
- `error.rs`: `DebuggerError`. `events.rs`: `DebuggerEvent` (only parked `ferros-ui` uses it).

## Fast loop
```sh
cargo test -p ferros-core session::tests::   # Session unit tests
cargo test -p ferros-core symbols::lines::   # one module
just test-crate ferros-core                  # whole crate, any OS
```
Engine behaviour still needs an integration test in `crates/ferros-tests` (root Rule).
