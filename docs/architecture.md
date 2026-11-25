# Ferros Architecture Guide

This document provides a comprehensive guide to Ferros' architecture, designed to help new contributors understand how the debugger works from the ground up. Read this document from top to bottom to build a complete mental model of the system.

## Table of Contents

1. [Introduction](#introduction)
2. [High-Level Architecture](#high-level-architecture)
3. [Workspace Structure](#workspace-structure)
4. [Core Components](#core-components)
   - [ferros (CLI)](#ferros-cli)
   - [ferros-core](#ferros-core)
   - [ferros-ui](#ferros-ui)
   - [ferros-protocol](#ferros-protocol)
   - [ferros-mir](#ferros-mir)
   - [ferros-utils](#ferros-utils)
5. [Data Flow](#data-flow)
6. [Platform Boundary](#platform-boundary)
7. [Error Handling](#error-handling)
8. [Future Architecture](#future-architecture)

---

## Introduction

Ferros is a Rust-native debugger built as a workspace of multiple crates. The verified engine launches a macOS arm64 process, stops, steps, and prints a backtrace. Ownership, lifetimes, and MIR are goals, not current behavior: `ferros-mir` is an empty crate, and the engine does not track borrows or lifetimes. Open design choices are recorded in [`docs/adr/`](adr/README.md).

The architecture follows a **layered design** where each crate has a specific responsibility, allowing for clean separation of concerns and easy extensibility.

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      User Interface Layer                   │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐       │
│  │   ferros     │  │  ferros-ui   │  │ ferros-mir   │       │
│  │   (CLI)      │  │ (TUI, parked)│  │  (empty)     │       │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘       │
└─────────┼─────────────────┼─────────────────┼───────────────┘
          │                 │                 │
          └─────────────────┼─────────────────┘
                            │
┌───────────────────────────┼──────────────────────────────────┐
│                    Core Debugging Layer                      │
│  ┌────────────────────────────────────────────────────────┐  │
│  │              ferros-core                               │  │
│  │  ┌─────────┐  ┌──────────┐  ┌──────────┐               │  │
│  │  │Platform │  │Breakpoint│  │ Symbols  │               │  │
│  │  │  Abst.  │  │  Mgmt    │  │  & DWARF │               │  │
│  │  └────┬────┘  └────┬─────┘  └────┬─────┘               │  │
│  │       │            │             │                     │  │
│  │  ┌────┴────────────┴─────────────┴───────┐             │  │
│  │  │       Session and Backend              │             │  │
│  │  └───────────────────────────────────────┘             │  │
│  └────────────────────────────────────────────────────────┘  │
└───────────────────────────┬──────────────────────────────────┘
                            │
┌───────────────────────────┼──────────────────────────────────┐
│                  Platform Implementation Layer               │
│  ┌──────────────┐  ┌───────────────────────────────────┐     │
│  │   macOS      │  │  Linux, Windows: no crate yet     │     │
│  │  (Mach APIs) │  │  (see docs/adr/0001-...)          │     │
│  └──────────────┘  └───────────────────────────────────┘     │
└──────────────────────────────────────────────────────────────┘
```

The architecture is divided into three main layers:

1. **User Interface Layer**: the `ferros` CLI. The TUI (`ferros-ui`) is parked, and `ferros-mir` is empty.
2. **Core Debugging Layer**: the portable `Session` engine and the `Backend` trait
3. **Platform Implementation Layer**: `ferros-macos` only. Linux and Windows are not crates yet.

---

## Workspace Structure

Ferros is organized as a Cargo workspace with the following crates:

```
ferros/
├── crates/
│   ├── ferros/          # Command-line interface
│   ├── ferros-core/     # Portable debugging engine (no OS-specific code)
│   ├── ferros-macos/    # macOS implementation (Mach APIs)
│   ├── ferros-tests/    # Integration test suite, harness, and fixtures
│   ├── ferros-utils/    # Shared utilities & structured logging
│   ├── ferros-mir/      # MIR integration & analysis (reserved)
│   ├── ferros-protocol/ # Communication & remote protocol (reserved)
│   └── ferros-ui/       # Terminal User Interface (parked while being rebuilt)
```

Each crate has a specific responsibility and can be used independently or together. The workspace configuration is defined in the root [`Cargo.toml`](../Cargo.toml).

---

## Core Components

### ferros (CLI)

**Location**: [`crates/ferros/`](../crates/ferros/)

The `ferros` crate provides the command-line interface for the debugger. It is the entry point for users and handles command parsing, logging initialization, and routing commands to `Session`.

#### Main Entry Point

The main function in [`crates/ferros/src/main.rs`](../crates/ferros/src/main.rs) parses the command line with `clap`, initializes file-only logging (`--log-level`, `--log-format`; nothing is logged to the terminal), creates the platform backend and hands one `Session` to the REPL in [`crates/ferros/src/repl/`](../crates/ferros/src/repl/).

#### Command Structure

- **`launch [--batch FILE] [--command CMD]... <program> [-- args...]`**: launch a process stopped at entry and open a session on it
- **`attach [--batch FILE] [--command CMD]... <pid>`**: attach to a running process and open a session on it
- **`find-logs`**: print the path to the Ferros log directory

The session reads commands interactively (rustyline), from a pipe (with a `(ferros) ` prompt per line) or from the batch list. Commands are registered in [`crates/ferros/src/repl/commands/mod.rs`](../crates/ferros/src/repl/commands/mod.rs), one module per area; see [`crates/ferros/README.md`](../crates/ferros/README.md) for the list.

#### Design Decisions

- **Separation of concerns**: CLI parsing and the REPL are separate from debugging engine logic
- **Platform abstraction**: CLI only names the platform backend in `crates/ferros/src/platform.rs`
- **Deterministic output**: the session logs to a file only, so batch transcripts can be compared in tests

---

### ferros-core

**Location**: [`crates/ferros-core/`](../crates/ferros-core/)

The `ferros-core` crate contains the portable debugging engine. `Session` applies debugging policy through the `Backend` mechanism interface; `Arch` contains CPU knowledge. Platform implementations live in their own crates.

#### Module Structure

```
ferros-core/src/
├── lib.rs              # Public API and re-exports, #![forbid(unsafe_code)]
├── session/            # Session: the portable engine (policy)
│   ├── mod.rs          # State machine, thread selection, registers, memory
│   ├── lifecycle.rs    # launch, attach, detach, resume, wait
│   ├── breakpoints.rs  # Software breakpoint table and logical breakpoints
│   ├── stepping.rs     # Resumable, serial software-breakpoint step-overs
│   ├── line_stepping.rs# Source-line step over, into, and out
│   ├── location.rs     # file:line, function, and address breakpoint locations
│   ├── stack.rs        # Stack traces and symbolication
│   ├── images.rs       # Loading the backend's image list into the symbol cache
│   └── exits.rs        # Non-local exits (panic landing pads, longjmp)
├── runtime/            # TargetRuntime: exit sources and signal frames (platform-supplied)
├── backend/            # Backend trait (mechanism), StopEvent, LaunchSpec, MockBackend
├── arch/               # CPU knowledge independent of the OS
├── breakpoints/        # Breakpoint types and store
├── symbols/            # Symbol resolution, DWARF parsing, unwinding
├── types/              # Core type definitions
├── error.rs            # Error types
└── events.rs           # Event types

ferros-macos/src/       # macOS implementation (the only crate with OS code)
├── lib.rs
├── task/               # MacOSBackend: impl Backend
├── exception/          # Mach exception port loop, stop events
├── launch.rs           # posix_spawn with LaunchSpec
├── images.rs           # Backend::images (target dyld image list)
├── runtime.rs          # MacOSRuntime: libunwind/__longjmp exit hooks, _sigtramp frame
├── memory.rs
├── threads.rs
├── breakpoints.rs      # Hardware breakpoints and watchpoints (not on Session)
├── registers/
│   ├── arm64.rs
│   └── x86_64.rs       # carried along, untested
└── ...
```

`Session::new(backend: Box<dyn Backend>, format: Arc<dyn ObjectFormat>)` creates a session, and `Session::with_runtime(Arc<dyn TargetRuntime>)` gives it the platform's runtime knowledge: which system libraries and entry symbols a thread leaves frames through (unwinder resumes, `longjmp`), and how a signal trampoline's saved registers are found. `ferros_macos::new_session()` pairs `MacOSBackend`, `MachOFormat`, and `MacOSRuntime` on macOS. See [Platform Boundary](#platform-boundary) for how the pieces divide the work.

#### Platform Factory

The CLI factory is `crates/ferros/src/platform.rs`:

```rust
pub fn create_session() -> ferros_core::Result<ferros_core::Session>
```

It is the only CLI module that names a platform crate and the only `cfg(target_os)` switch outside platform and test crates.

#### Error Handling

Session and backend operations return the crate result type. The error types are defined in [`crates/ferros-core/src/error.rs`](../crates/ferros-core/src/error.rs) and use `thiserror` for structured error handling.

**Common Error Types**:
- `NotAttached`: Operation requires an attached process
- `AttachFailed`: Failed to attach to process
- `PermissionDenied`: Insufficient permissions
- `InvalidArgument`: Invalid parameter
- `Io`: I/O error (file operations, memory access, etc.)

#### Stop channel

A stop is synchronous. `Session::wait` returns a [`StopEvent`](../crates/ferros-core/src/backend/types.rs) (`Breakpoint`, `SingleStep`, `Signal`, `Interrupted`, `Exited`, `Killed`, or `Other`). Backends produce that enum. They do not publish `DebuggerEvent`.

`DebuggerEvent` (`TargetStopped`, `TargetResumed`) in [`crates/ferros-core/src/events.rs`](../crates/ferros-core/src/events.rs) is not published by any backend. The parked `ferros-ui` crate is the only caller. It is not the live stop channel.

#### Breakpoint Management

`Session` installs software breakpoints: it reads the original instruction, writes `Arch::trap_instruction`, and records the original bytes. Hardware breakpoints and watchpoints are not `Session` operations (`BackendError::Unsupported`). Unverified debug-register helpers remain inside `ferros-macos` and are not on `Backend`.

**Key Types**:
- `BreakpointRequest`: Request to create a breakpoint. Only the `Software` variant is installed by `Session`.
- `BreakpointId`: Unique identifier for a breakpoint
- `BreakpointInfo`: Public information about a breakpoint
- `BreakpointStore`: The session's address-keyed table. It is a `HashMap` with `&mut self` methods, not a thread-safe store. The session uses it on the one thread that owns the backend.

**Code Reference**: [`crates/ferros-core/src/breakpoints/mod.rs`](../crates/ferros-core/src/breakpoints/mod.rs) and [`crates/ferros-core/src/session/breakpoints.rs`](../crates/ferros-core/src/session/breakpoints.rs)

#### Symbol Resolution

The `symbols` module provides symbol resolution, DWARF parsing, and type introspection. It uses the `gimli` crate to parse DWARF debugging information.

**Key Components**:
- **`SymbolCache`**: Caches symbol information for binary images
- **`Symbolication`**: Resolves addresses to function names and source locations
- **`TypeExtractor`**: Extracts type information from DWARF
- **`BinaryImage`**: Represents a loaded binary and its DWARF sections

**Code Reference**: [`crates/ferros-core/src/symbols/mod.rs`](../crates/ferros-core/src/symbols/mod.rs)

On macOS, `ferros_macos::MachOFormat` (the session's `ObjectFormat`; `ferros-core` only
consumes the DWARF bytes it returns) discovers debug information in this order: embedded DWARF, an adjacent
`<executable>.dSYM` with matching UUID and architecture, a validated UUID/architecture
cache entry, then `dsymutil`. Companion member names are not used as identity.
Generated DWARF is validated before an atomic cache publication, with a 30-second
per-tool deadline and cleanup on failure. The cache defaults to
`$XDG_CACHE_HOME/ferros/debuginfo`, or `$HOME/.cache/ferros/debuginfo`, falling back to
the temporary directory when neither variable is set. `ferros_macos::DebugInfoOptions` configures
these locations, tool selection (including disabling it), and the deadline through
`MachOFormat::with_debug_info_options`, passed to `SymbolCache::new`, `Session::new` or
`Session::with_object_format`.

`Session::symbolicate` returns `DebuggerError::NoDebugInfo` for a mapped image without
usable debug information, with `split-debuginfo` and `dsymutil` remedies. Stack traces
remain best effort and can return raw frames. Debug-map object files must still exist
for `dsymutil` to succeed on an unpacked Cargo build; Ferros does not parse them itself.

The macOS backend reads `TASK_DYLD_INFO` and the stopped target's dyld image array.
Before dyld initializes that array at the initial suspended launch stop, it locates
only the main executable's mapped header so pre-execution breakpoints still work.
Session combines loader header addresses with executable segment metadata for checked
runtime/file virtual-address conversions, excludes `__PAGEZERO` and segment gaps, and
prunes unloaded images on refresh. External DWARF never supplies runtime layout or
`__eh_frame`. Images present only in the system shared cache can be enumerated without
loading their debug information; system-library debug symbols remain out of scope.


#### Type System

The `types` module defines core types used throughout Ferros:

- **`ProcessId`**: Process identifier (newtype wrapper)
- **`ThreadId`**: Thread identifier (platform-specific)
- **`Address`**: Memory address (newtype wrapper)
- **`Registers`**: CPU register state
- **`StackFrame`**: Stack frame information
- **`MemoryRegion`**: Memory region (segment) information
- **`StopReason`**: Reason why process stopped
- **`Architecture`**: CPU architecture (ARM64, x86_64, etc.)

**Code Reference**: [`crates/ferros-core/src/types/mod.rs`](../crates/ferros-core/src/types/mod.rs)

#### Platform Implementation (macOS)

The `ferros-macos` crate implements `Backend` using macOS process and Mach APIs. `MacOSBackend` is its public backend type. `Backend::images` delegates to `crates/ferros-macos/src/images.rs`.

---

### ferros-ui

**Location**: [`crates/ferros-ui/`](../crates/ferros-ui/)

The `ferros-ui` crate provides a Terminal User Interface (TUI) built with `ratatui`. It provides an interactive debugging experience with multiple views.

#### Module Structure

```
ferros-ui/src/
├── lib.rs        # Public API
├── app.rs        # Application state and logic
├── tui.rs        # TUI initialization and event loop
├── ui.rs         # UI rendering
├── widgets.rs    # Custom widgets
└── event.rs      # Event handling
```

#### Application State

The `App` struct in [`crates/ferros-ui/src/app.rs`](../crates/ferros-ui/src/app.rs) manages the application state:

- **Debugger instance**: the parked TUI still holds the old `Debugger` trait object; it has not been ported to `Session` and is excluded from the workspace
- **View modes**: Overview, Registers, Threads, Memory Regions, Output
- **Table states**: For navigating registers, threads, memory regions
- **Process output**: Captured stdout/stderr from launched processes
- **Stop events**: a log of `DebuggerEvent` values. No backend publishes that enum; `Session::wait` is the live stop channel

**Code Reference**: [`crates/ferros-ui/src/app.rs`](../crates/ferros-ui/src/app.rs)

#### View Modes

The TUI supports multiple view modes:

1. **Overview**: General debugger information and status
2. **Registers**: CPU register values
3. **Threads**: List of threads in the process
4. **Memory Regions**: Memory map of the process
5. **Output**: Process stdout/stderr output

**Code Reference**: [`crates/ferros-ui/src/app.rs`](../crates/ferros-ui/src/app.rs)

#### Event Loop

The TUI event loop in [`crates/ferros-ui/src/tui.rs`](../crates/ferros-ui/src/tui.rs) handles:

- **Keyboard input**: User commands (suspend, resume, navigate, quit)
- **Terminal rendering**: Updates the display on each tick
- **Background tasks**: Reading process output and forwarding `DebuggerEvent` values, which the current backends do not send
- **Cleanup**: Detaching from process and restoring terminal state

**Key Features**:
- Async event handling with Tokio
- Non-blocking process output reading
- Debugger event forwarding
- Graceful shutdown with cleanup

**Code Reference**: [`crates/ferros-ui/src/tui.rs:70-172`](../crates/ferros-ui/src/tui.rs#L70-L172)

#### Keyboard Commands

- **`q`/`Esc`**: Quit
- **`1-5`**: Switch view modes
- **`s`**: Suspend process
- **`r`**: Resume process
- **`↑`/`↓`**: Navigate tables
- **`Ctrl+A`**: Attach to process (future)

**Code Reference**: [`crates/ferros-ui/src/app.rs:175-236`](../crates/ferros-ui/src/app.rs#L175-L236)

---

### ferros-protocol

**Location**: [`crates/ferros-protocol/`](../crates/ferros-protocol/)

The `ferros-protocol` crate is currently a placeholder for future remote debugging capabilities. It will define structured messages for communication between the debugger and frontends (TUI, GUI, or remote clients).

**Future Design**:
- JSON or binary (MessagePack) message format
- Protocol versioning for backward compatibility
- Support for remote debugging over network

**Code Reference**: [`crates/ferros-protocol/src/lib.rs`](../crates/ferros-protocol/src/lib.rs)

---

### ferros-mir

**Location**: [`crates/ferros-mir/`](../crates/ferros-mir/)

The `ferros-mir` crate is currently a placeholder for MIR (Mid-level Intermediate Representation) integration. This will enable debugging at the Rust compiler's intermediate representation level.

**Future Design**:
- MIR block visualization
- Symbolic execution of MIR
- Type and lifetime introspection
- Integration with `rustc` internals

**Code Reference**: [`crates/ferros-mir/src/lib.rs`](../crates/ferros-mir/src/lib.rs)

---

### ferros-utils

**Location**: [`crates/ferros-utils/`](../crates/ferros-utils/)

The `ferros-utils` crate provides shared utilities used across the workspace, primarily logging infrastructure.

#### Logging Module

The logging module in [`crates/ferros-utils/src/logging.rs`](../crates/ferros-utils/src/logging.rs) provides structured logging using `tracing`:

**Features**:
- Multiple output formats (pretty, JSON)
- Environment variable configuration (`RUST_LOG`, `FERROS_LOG_FORMAT`)
- File and console logging
- File-only logging for the CLI and the TUI (keeps stdout deterministic)

**The CLI session**: `ferros launch` and `ferros attach` call `start_file_logging`, which opens `YYYY-MM-DD-ferros.log` in `log_directory()` and returns a `LoggingSession`. The session owns the background worker that writes the file; `main` holds it until it returns, so the records written just before exit reach the file. Dropping the session flushes within a bound (about 100 ms to queue the shutdown, then up to about one second to drain); a worker still behind after that is abandoned, so exit never hangs on the log file.

**The log directory**: `log_directory()` is `~/.ferros` when `HOME` is set and not empty, otherwise the OS temporary directory, always made absolute (an empty `TMPDIR` means `/tmp`, a relative one is joined onto the working directory). The logger opens exactly that path, and `ferros find-logs` prints its raw OS bytes, so the printed directory is the one the logger opened.

**Key Functions**:
- `start_file_logging()`: File-only logging for a CLI session; returns the `LoggingSession` to hold until exit
- `log_directory()`: The directory every file logger opens (what `ferros find-logs` prints)
- `init_logging()`: Initialize with environment variables
- `init_logging_with_level()`: Initialize with explicit level/format
- `init_logging_for_tui()`, `init_logging_to_file()`: File-only logging written synchronously, for callers that cannot hold a session; nothing needs flushing at exit

Every initializer installs the tracing subscriber first and the `log` bridge second; an already installed `log` logger keeps the `log` records and tracing still works.

**Code Reference**: [`crates/ferros-utils/src/logging.rs`](../crates/ferros-utils/src/logging.rs)

---

## Data Flow

### Launching a Process

1. **User invokes**: `ferros launch ./target/debug/my_program`
2. **CLI parses command**: [`crates/ferros/src/main.rs`](../crates/ferros/src/main.rs)
3. **Create session**: `create_session()` pairs the platform backend with its object format (`Session::new`)
4. **Launch process**: `session.launch()` spawns process in suspended state
5. **Run commands**: the REPL drives the session (`continue`, `break`, `registers`, ...) until `quit` or the end of input
6. **End**: a launched process is killed with `session.kill()`; an attached one is released with `session.detach()`

### Attaching to a Process

1. **User invokes**: `ferros attach <pid>`
2. **CLI parses command**: [`crates/ferros/src/main.rs`](../crates/ferros/src/main.rs)
3. **Create session**: `create_session()` supplies the platform mechanism and object format
4. **Attach to process**: `session.attach(pid)` asks the backend to stop the process. On macOS that takes a Mach task port. There is no Linux backend.
5. **Run commands & detach**: the REPL drives the session; at `quit` or the end of input `session.detach()` releases the process, which keeps running

### Reading Registers

1. **User or UI requests registers**: via `session.read_registers()`
2. **Session delegates**: reads register state for the selected or target thread via `Backend::read_registers(thread)`
3. **Platform implementation**:
   - macOS: `thread_get_state()` with `ARM_THREAD_STATE64` (the x86_64 flavor is carried and untested)
   - Linux: no backend yet (see [ADR 0001](adr/0001-object-file-boundary.md))
4. **Return registers**: `Registers` struct with all CPU registers
5. **Session exposes**: `registers.pc`, general-purpose registers, floating-point and vector registers

### Breakpoint Flow

1. **User requests breakpoint**: Via CLI or API
2. **Create request**: `BreakpointRequest::Software { address }`
3. **Add breakpoint**: `session.add_breakpoint(request)`
4. **Session installs**: reads the original instruction with `Backend::read_memory`, writes the trap from `Arch::trap_instruction` with `Backend::write_memory`, and stores the original bytes in its table
5. **Process hits breakpoint**: the kernel delivers an exception to the backend, which stops every thread
6. **Session receives event**: `Session::wait` returns `StopEvent::Breakpoint { thread, pc }` and records the hit; memory reads through `Session::read_memory` transparently hide the trap bytes
7. **Step over on resume**: `Session::resume` coordinates serial step-over (`stepping.rs`), restoring the original bytes, executing one instruction via hardware single-step while keeping other threads suspended, re-arming the trap, and resuming execution

---

## Platform Boundary

All operating-system code sits behind one trait. Three pieces of `ferros-core` define the boundary:

| Piece | Role | Where |
|-------|------|-------|
| `Backend` | Mechanism: control one process, read and write its registers and memory, list threads, regions and images, and report why it stopped (`StopEvent`). | [`crates/ferros-core/src/backend/`](../crates/ferros-core/src/backend/) |
| `Session` | Policy: the process state machine (not started, running, stopped, exited), thread selection, the software breakpoint table, stack traces and symbolication. It owns a `Box<dyn Backend>` and works with any backend. | [`crates/ferros-core/src/session/`](../crates/ferros-core/src/session/) |
| `Arch` | CPU knowledge that does not depend on the OS: trap bytes, which register is the pc, how to map the pc of a trap stop. | [`crates/ferros-core/src/arch/`](../crates/ferros-core/src/arch/) |

A backend never interprets what it is asked to do. Software breakpoints show the split: `Session` reads the original bytes with `Backend::read_memory`, writes `Arch::trap_instruction` with `Backend::write_memory`, and keeps the original bytes in its own table. The backend knows nothing about breakpoints; it reports a trap as `StopEvent::Breakpoint` with the raw pc, and `Session` maps that pc with `Arch::pc_after_trap` and counts the hit. The state machine and the full contract of every call are documented on the `Backend` trait and in the `session` module.

Two backends exist:

- `ferros_macos::MacOSBackend` in [`crates/ferros-macos/`](../crates/ferros-macos/) uses Mach process control (`task_for_pid`, thread state, `mach_vm` memory calls, Mach exception ports) and `posix_spawn`. `Backend::images` delegates to `src/images.rs`, which reads the stopped target's dyld image list (every image dyld published, plus dyld itself). Before dyld has filled that list, the bootstrap stop reports the main executable so breakpoints can be planted. Hardware breakpoints and watchpoints are not part of `Backend` or `Session`; that code stays inside the macOS crate and is not verified.
- `MockBackend` in `ferros_core::backend::mock` is a scripted, in-memory backend. It is compiled for `ferros-core`'s own tests and, through the `test-util` feature, for other crates' tests. The `Session` unit tests run against it on any OS, including the Ubuntu CI job.

### Rules

1. `ferros-core` has no OS dependency (`mach2`, `libproc`, `libc`, a platform crate), no `cfg(target_os)` and no `unsafe`. It declares `#![forbid(unsafe_code)]`.
2. `unsafe` is allowed only in a platform crate, for raw OS bindings.
3. The CLI names a platform crate in exactly one module, [`crates/ferros/src/platform.rs`](../crates/ferros/src/platform.rs). That module is also the only `cfg(target_os)` switch outside the platform and test crates.
4. `ferros-core` names no system library and no kernel signal-frame layout: they come from the platform's `runtime::TargetRuntime` ([#201](https://github.com/JamalLyons/ferros/issues/201)).
5. Engine behaviour is tested twice: against `MockBackend` in `ferros-core`, and against real fixture processes in `crates/ferros-tests`.

The rules are enforced by `just check-boundary` ([`scripts/check-boundary.sh`](../scripts/check-boundary.sh)), which CI runs on both the Ubuntu and the macOS runner, and by [`crates/ferros-tests/tests/platform_boundary.rs`](../crates/ferros-tests/tests/platform_boundary.rs).

### How to Add a Platform

1. Create a new crate, for example `crates/ferros-linux`, that depends on `ferros-core` and implements `Backend` for one type, `ferros_core::objfile::ObjectFormat` for its object-file format, and `ferros_core::runtime::TargetRuntime` for its system libraries and signal frames. Follow the state machine and error rules documented on the trait: an illegal call returns `BackendError::InvalidState`, an error never changes the state, and memory is returned exactly as it is in the target. Gate the crate with `#![cfg(target_os = "...")]` so it compiles as an empty crate elsewhere, as `ferros-macos` does.
2. Add the crate as a target-specific dependency of `crates/ferros` and add one arm to the factory:

```rust
pub fn create_session() -> ferros_core::Result<Session>
{
    #[cfg(target_os = "macos")]
    {
        ferros_macos::new_session()
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Session::new(Box::new(ferros_linux::LinuxBackend::new()?), ferros_linux::ElfFormat::shared())
            .with_runtime(ferros_linux::LinuxRuntime::shared()))
    }
    // ...
}
```

3. Add integration tests for the platform in `crates/ferros-tests` that debug the fixture programs through `Session`.

`Session`'s process-control policy (state machine, software breakpoints, stepping) works with any `Backend`, and its symbolication works with any `objfile::ObjectFormat`. Per [ADR 0001](adr/0001-object-file-boundary.md) ([#193](https://github.com/JamalLyons/ferros/issues/193)), the Mach-O implementor (headers, universal slices, symbol tables, compact unwind, dSYM discovery) is `ferros_macos::macho::MachOFormat`; `LoadedImage.build_id` holds up to 32 bytes, so an ELF build-id fits. DWARF stays in core. A new platform implements `Backend`, `ObjectFormat`, and `TargetRuntime` ([#201](https://github.com/JamalLyons/ferros/issues/201)); `ferros-core` names no system library and no kernel signal-frame layout. A Linux or Windows crate implements the trait for its own format and does not receive Mach-O code, and `ferros-core` does not receive ELF or PE. If the new platform needs something the `Backend` trait cannot express, extend the trait (and `MockBackend`) rather than reaching around it.

---

## Error Handling

Ferros uses structured error handling with `thiserror` for library errors and `anyhow` for application errors.

### Error Types

**Library Errors** (`ferros-core`): Use `thiserror` for structured error types
- `DebuggerError`: Enum of all possible debugger errors
- Each variant includes context and source errors

**Application Errors** (`ferros`, `ferros-ui`): Use `anyhow` for flexible error handling
- Context chains for debugging
- Easy error propagation

### Error Propagation

Errors are propagated using the `?` operator:

```rust
let regs = session.read_registers()?;
```

**Code Reference**: [`crates/ferros-core/src/error.rs`](../crates/ferros-core/src/error.rs)

---

## Future Architecture

### MIR Integration

The `ferros-mir` crate will integrate with Rust compiler internals to provide:
- MIR block visualization
- Symbolic execution
- Type and lifetime introspection
- Integration with `rustc_driver` or `rustc_private`

### Remote Debugging

The `ferros-protocol` crate will enable:
- Network-based debugging
- Language Server Protocol (LSP) integration
- IDE plugin support
- Multi-process debugging

### Performance Optimizations

- Caching of symbol information
- Lazy loading of DWARF sections
- Efficient memory region enumeration
- Optimized breakpoint management

---

## Conclusion

Ferros is designed with a clear separation of concerns, allowing each component to be developed and tested independently. The `Backend` mechanism interface and `Session` policy engine support platform implementations behind a stable boundary.

For contributors, the key entry points are:
- **CLI**: [`crates/ferros/src/main.rs`](../crates/ferros/src/main.rs)
- **Core API**: [`crates/ferros-core/src/session/`](../crates/ferros-core/src/session/)
- **Platform Implementation**: [`crates/ferros-macos/src/`](../crates/ferros-macos/src/)
- **TUI**: [`crates/ferros-ui/src/app.rs`](../crates/ferros-ui/src/app.rs)

Each module is well-documented with Rustdoc comments, and the codebase follows Rust best practices for safety, performance, and maintainability.

