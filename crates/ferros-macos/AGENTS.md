# ferros-macos map

macOS `Backend`, runtime, and all Mach-O knowledge. The only crate that allows `unsafe`. Root [`AGENTS.md`](../../AGENTS.md) Conventions bind here; this file only says where things are. The crate is `#![cfg(target_os = "macos")]`: it compiles empty elsewhere.

Open first: `src/lib.rs` (launch, stops, resume, permissions), then `src/task/mod.rs`.

## Modules (`src/`)
- `task/`: `MacOSBackend`. `mod.rs`: struct and inherent API; `backend.rs`: `Backend` impl and `Drop`; `handler.rs`, `thread_actions.rs`, `ptrace.rs`, `state.rs`: helpers.
- `exception/`: the Mach exception loop. `run.rs`: loop and release of held exceptions; `messages.rs`: exception-port messages and replies; `stops.rs`: exceptions to stop records; `event.rs`: exception to `StopEvent`; `suspend.rs`: batches held behind a failed `task_suspend`; `fault_injector.rs`: test-only faults.
- `birth.rs`: holds threads born during a resume (pthread entry traps).
- `memory.rs`: `vm_read`/`vm_write`, regions, protection.
- `images.rs`: `Backend::images`, the dyld image list.
- `macho/`: `MachOFormat`, the `ObjectFormat` implementor. `image.rs`: segment layout and slices; `symtab.rs`: `LC_SYMTAB` names; `debuginfo.rs`: DWARF discovery (embedded, `.dSYM`); `compact_unwind.rs`: `__unwind_info`; `tests.rs`: through a `Session` on the mock backend.
- `runtime.rs`: `MacOSRuntime` (libunwind, `__longjmp`, `_sigtramp` frame).
- `registers/`: thread-state access (`arm64.rs`, `x86_64.rs`, `debug.rs` for debug registers).
- `launch.rs`: `posix_spawn`. `lifecycle.rs`: death notification and reaping. `threads.rs`: thread listing.
- `breakpoints.rs`: hardware breakpoints/watchpoints. `guards.rs`: RAII guards. `terminal.rs`: job control for the CLI.
- `ffi.rs`, `constants.rs`, `types.rs`, `error.rs`, `diagnostics.rs`, `file_identity.rs`: bindings and support.

## Loop
Unit tests: `cargo test -p ferros-macos`. Behaviour is proven by `crates/ferros-tests/tests/native/` (`cargo test -p ferros-tests --test native <area>::`). macOS only; a Linux checkout cannot build or test this crate.
