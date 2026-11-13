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

Ferros is a Rust-native debugger built as a workspace of multiple crates. Unlike traditional debuggers that retrofit C/C++ semantics onto Rust programs, Ferros is designed from the ground up to understand Rust's unique features:

- **Ownership and Borrowing**: Understand Rust's memory model
- **Lifetimes**: Track variable lifetimes through the execution
- **MIR Integration**: Debug at the compiler's intermediate representation level
- **Type System**: Introspect Rust types and their relationships

The architecture follows a **layered design** where each crate has a specific responsibility, allowing for clean separation of concerns and easy extensibility.

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      User Interface Layer                   │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐       │
│  │   ferros     │  │  ferros-ui   │  │ ferros-mir   │       │
│  │   (CLI)      │  │   (TUI)      │  │  (MIR)       │       │
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
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐        │
│  │   macOS      │  │    Linux     │  │   Windows    │        │
│  │  (Mach APIs) │  │   (ptrace)   │  │ (WinDbg API) │        │
│  └──────────────┘  └──────────────┘  └──────────────┘        │
└──────────────────────────────────────────────────────────────┘
```

The architecture is divided into three main layers:

1. **User Interface Layer**: Command-line interface, TUI, and MIR analysis tools
2. **Core Debugging Layer**: Platform-agnostic debugging primitives and abstractions
3. **Platform Implementation Layer**: Platform-specific implementations (macOS, Linux, Windows)

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

The main function in [`crates/ferros/src/main.rs`](../crates/ferros/src/main.rs) handles:

1. **Command Parsing**: Uses `clap` to parse command-line arguments and subcommands
2. **Logging Initialization**: Sets up `tracing` structured logging based on CLI flags (`--log-level`, `--log-format`) or environment variables
3. **Mode Normalization**: Normalizes non-headless attach and launch to headless mode while the interactive TUI is being rebuilt
4. **Command Execution**: Calls `run_command` to drive `Session` through the platform backend

#### Command Structure

The CLI supports several commands:

- **`attach <pid> [--headless]`**: Attach to a running process
- **`launch <program> [args...] [--headless]`**: Launch a new process under debugger control
- **`registers`**: Display CPU registers (requires persistent session)
- **`memory <address> [--length <N>]`**: Read memory from process
- **`regions`**: List memory regions of the target process
- **`threads`**: List all threads
- **`suspend`/`resume`**: Control process execution
- **`detach`**: Detach from the target process
- **`info`**: Show target status, architecture, and debugger information
- **`find-logs`**: Print the path to the Ferros log directory

#### Key Code References

- **Main entry point**: [`crates/ferros/src/main.rs:86`](../crates/ferros/src/main.rs#L86)
- **Command definitions**: [`crates/ferros/src/main.rs:33-84`](../crates/ferros/src/main.rs#L33-L84)
- **Command execution**: [`crates/ferros/src/main.rs:145-212`](../crates/ferros/src/main.rs#L145-L212)
- **Debugger info printer**: [`crates/ferros/src/main.rs:214-234`](../crates/ferros/src/main.rs#L214-L234)

#### Design Decisions

- **Separation of concerns**: CLI parsing is separate from debugging engine logic
- **Platform abstraction**: CLI only names the platform backend in `crates/ferros/src/platform.rs`
- **Logging flexibility**: Supports file and console logging in pretty and JSON formats

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
│   ├── breakpoints.rs  # Software breakpoint table
│   ├── stepping.rs     # Resumable, serial software-breakpoint step-overs
│   ├── stack.rs        # Stack traces and symbolication
│   └── images.rs       # Loading the backend's image list into the symbol cache
├── backend/            # Backend trait (mechanism), StopEvent, LaunchSpec, MockBackend
├── arch/               # CPU knowledge independent of the OS
├── breakpoints/        # Breakpoint types and store
├── symbols/            # Symbol resolution, DWARF parsing, unwinding
├── types/              # Core type definitions
├── error.rs            # Error types
└── events.rs           # Event types

ferros-macos/src/       # macOS implementation (the only crate with OS code)
├── lib.rs
├── task.rs             # MacOSBackend: impl Backend
├── exception.rs        # Mach exception port loop, stop events
├── launch.rs           # posix_spawn with LaunchSpec
├── images.rs           # Backend::images (main executable only for now)
├── memory.rs
├── threads.rs
├── breakpoints.rs      # Hardware breakpoints and watchpoints (not on Session)
├── registers/
│   ├── arm64.rs
│   └── x86_64.rs       # carried along, untested
└── ...
```

`Session::new(backend: Box<dyn Backend>)` creates a session. See [Platform Boundary](#platform-boundary) for how the pieces divide the work.

#### Platform Factory

The CLI factory is `crates/ferros/src/platform.rs`:

```rust
pub fn create_backend() -> ferros_core::Result<Box<dyn ferros_core::Backend>>
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

#### Event System

The event system allows debugger backends to notify higher layers about asynchronous state changes (e.g., process stopped, breakpoint hit) without polling. Events are defined in [`crates/ferros-core/src/events.rs`](../crates/ferros-core/src/events.rs).

**Event Types**:
- `TargetStopped { reason, thread }`: Process stopped for a reason
- `TargetResumed`: Process resumed execution

**Code Reference**: [`crates/ferros-core/src/events.rs:14-27`](../crates/ferros-core/src/events.rs#L14-L27)

#### Breakpoint Management

Breakpoints are managed through the `breakpoints` module. The system supports:

- **Software breakpoints**: Implemented via trap instructions (BRK on ARM, INT3 on x86)
- **Hardware breakpoints**: Using CPU debug registers
- **Watchpoints**: Data breakpoints that trigger on memory access

**Key Types**:
- `BreakpointRequest`: Request to create a breakpoint
- `BreakpointId`: Unique identifier for a breakpoint
- `BreakpointInfo`: Public information about a breakpoint
- `BreakpointStore`: Thread-safe storage for breakpoint state

**Code Reference**: [`crates/ferros-core/src/breakpoints/mod.rs`](../crates/ferros-core/src/breakpoints/mod.rs)

#### Symbol Resolution

The `symbols` module provides symbol resolution, DWARF parsing, and type introspection. It uses the `gimli` crate to parse DWARF debugging information.

**Key Components**:
- **`SymbolCache`**: Caches symbol information for binary images
- **`Symbolication`**: Resolves addresses to function names and source locations
- **`TypeExtractor`**: Extracts type information from DWARF
- **`BinaryImage`**: Represents a loaded binary and its DWARF sections

**Code Reference**: [`crates/ferros-core/src/symbols/mod.rs`](../crates/ferros-core/src/symbols/mod.rs)

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
- **Stop events**: Log of stop/resume events

**Code Reference**: [`crates/ferros-ui/src/app.rs:33-67`](../crates/ferros-ui/src/app.rs#L33-L67)

#### View Modes

The TUI supports multiple view modes:

1. **Overview**: General debugger information and status
2. **Registers**: CPU register values
3. **Threads**: List of threads in the process
4. **Memory Regions**: Memory map of the process
5. **Output**: Process stdout/stderr output

**Code Reference**: [`crates/ferros-ui/src/app.rs:70-83`](../crates/ferros-ui/src/app.rs#L70-L83)

#### Event Loop

The TUI event loop in [`crates/ferros-ui/src/tui.rs`](../crates/ferros-ui/src/tui.rs) handles:

- **Keyboard input**: User commands (suspend, resume, navigate, quit)
- **Terminal rendering**: Updates the display on each tick
- **Background tasks**: Reading process output, forwarding debugger events
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
- TUI mode file-only logging (prevents stdout interference)

**Key Functions**:
- `init_logging()`: Initialize with environment variables
- `init_logging_with_level()`: Initialize with explicit level/format
- `init_logging_for_tui()`: Initialize file-only logging for TUI

**Code Reference**: [`crates/ferros-utils/src/logging.rs:152-224`](../crates/ferros-utils/src/logging.rs#L152-L224)

---

## Data Flow

### Launching a Process

1. **User invokes**: `ferros launch ./target/debug/my_program`
2. **CLI parses command**: [`crates/ferros/src/main.rs:158-188`](../crates/ferros/src/main.rs#L158-L188)
3. **Create backend**: `create_backend()` returns the platform backend, then `Session::new` creates the engine session
4. **Launch process**: `session.launch()` spawns process in suspended state
5. **Show information**: the headless CLI prints the state, threads and memory regions while the process is stopped at entry
6. **Detach**: `session.detach()` releases the process, which then runs on its own (the interactive front end is being rebuilt)

### Attaching to a Process

1. **User invokes**: `ferros attach <pid>`
2. **CLI parses command**: [`crates/ferros/src/main.rs:148-157`](../crates/ferros/src/main.rs#L148-L157)
3. **Create backend**: `create_backend()` supplies the platform mechanism
4. **Attach to process**: `session.attach(pid)` gets Mach task port (macOS) or uses ptrace (Linux)
5. **Show information & detach**: while the interactive front end is being rebuilt, the CLI prints target state, threads, and memory regions, then detaches

### Reading Registers

1. **User or UI requests registers**: via `session.read_registers()`
2. **Session delegates**: reads register state for the selected or target thread via `Backend::read_registers(thread)`
3. **Platform implementation**: 
   - macOS: `thread_get_state()` with `ARM_THREAD_STATE64` (or x86 flavor)
   - Linux: `ptrace(PTRACE_GETREGSET)` / `PTRACE_GETREGS` (planned)
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

- `ferros_macos::MacOSBackend` in [`crates/ferros-macos/`](../crates/ferros-macos/) uses Mach process control (`task_for_pid`, thread state, `mach_vm` memory calls, Mach exception ports) and `posix_spawn`. `Backend::images` delegates to `src/images.rs`, which for now reports only the main executable. Hardware breakpoints and watchpoints are not part of `Backend` or `Session`; that code stays public inside the macOS crate.
- `MockBackend` in `ferros_core::backend::mock` is a scripted, in-memory backend. It is compiled for `ferros-core`'s own tests and, through the `test-util` feature, for other crates' tests. The `Session` unit tests run against it on any OS, including the Ubuntu CI job.

### Rules

1. `ferros-core` has no OS dependency (`mach2`, `libproc`, `libc`, a platform crate), no `cfg(target_os)` and no `unsafe`. It declares `#![forbid(unsafe_code)]`.
2. `unsafe` is allowed only in a platform crate, for raw OS bindings.
3. The CLI names a platform crate in exactly one module, [`crates/ferros/src/platform.rs`](../crates/ferros/src/platform.rs). That module is also the only `cfg(target_os)` switch outside the platform and test crates.
4. Engine behaviour is tested twice: against `MockBackend` in `ferros-core`, and against real fixture processes in `crates/ferros-tests`.

The rules are enforced by `just check-boundary` ([`scripts/check-boundary.sh`](../scripts/check-boundary.sh)), which CI runs on both the Ubuntu and the macOS runner, and by [`crates/ferros-tests/tests/platform_boundary.rs`](../crates/ferros-tests/tests/platform_boundary.rs).

### How to Add a Platform

1. Create a new crate, for example `crates/ferros-linux`, that depends on `ferros-core` and implements `Backend` for one type. Follow the state machine and error rules documented on the trait: an illegal call returns `BackendError::InvalidState`, an error never changes the state, and memory is returned exactly as it is in the target. Gate the crate with `#![cfg(target_os = "...")]` so it compiles as an empty crate elsewhere, as `ferros-macos` does.
2. Add the crate as a target-specific dependency of `crates/ferros` and add one arm to the factory:

```rust
pub fn create_backend() -> ferros_core::Result<Box<dyn Backend>>
{
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(ferros_macos::MacOSBackend::new()?))
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(ferros_linux::LinuxBackend::new()?))
    }
    // ...
}
```

3. Add integration tests for the platform in `crates/ferros-tests` that debug the fixture programs through `Session`.

Nothing in `ferros-core` changes: `Session`, the breakpoint table, unwinding and symbolication already work with any `Backend`. If the new platform needs something the trait cannot express, extend the trait (and `MockBackend`) rather than reaching around it.

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

