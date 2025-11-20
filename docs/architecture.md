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
6. [Platform Abstraction](#platform-abstraction)
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
│  │  │         Debugger Trait                │             │  │
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
│   ├── ferros-core/     # Low-level debugging primitives & process control
│   ├── ferros-mir/      # MIR integration & analysis
│   ├── ferros-ui/       # Terminal User Interface (TUI)
│   ├── ferros-protocol/ # Communication layer (future)
│   └── ferros-utils/     # Shared utilities & helpers
```

Each crate has a specific responsibility and can be used independently or together. The workspace configuration is defined in the root [`Cargo.toml`](../../Cargo.toml).

---

## Core Components

### ferros (CLI)

**Location**: [`crates/ferros/`](../../crates/ferros/)

The `ferros` crate provides the command-line interface for the debugger. It's the entry point for users and handles command parsing, logging initialization, and routing to either headless operations or the TUI.

#### Main Entry Point

The main function in [`crates/ferros/src/main.rs`](../../crates/ferros/src/main.rs) handles:

1. **Command Parsing**: Uses `clap` to parse command-line arguments
2. **Logging Initialization**: Sets up logging based on CLI flags or environment variables
3. **Mode Selection**: Chooses between headless mode (CLI-only) or TUI mode (interactive)
4. **Async Runtime**: Spawns a Tokio runtime for TUI operations

#### Command Structure

The CLI supports several commands:

- **`attach <pid>`**: Attach to a running process
- **`launch <program> [args...]`**: Launch a new process under debugger control
- **`registers`**: Display CPU registers (future - requires state management)
- **`memory <address> [length]`**: Read memory from process (future)
- **`threads`**: List all threads (future)
- **`suspend`/`resume`**: Control process execution (future)
- **`info`**: Show debugger information

#### Key Code References

- **Main entry point**: [`crates/ferros/src/main.rs:81`](../../crates/ferros/src/main.rs#L81)
- **Command definitions**: [`crates/ferros/src/main.rs:30-79`](../../crates/ferros/src/main.rs#L30-L79)
- **Async command handler**: [`crates/ferros/src/main.rs:164-230`](../../crates/ferros/src/main.rs#L164-L230)
- **Headless command handler**: [`crates/ferros/src/main.rs:232-358`](../../crates/ferros/src/main.rs#L232-L358)

#### Design Decisions

- **Separation of concerns**: CLI parsing is separate from debugging logic
- **Async-first**: TUI mode uses async/await for non-blocking operations
- **Logging flexibility**: Supports both file and console logging with different formats

---

### ferros-core

**Location**: [`crates/ferros-core/`](../../crates/ferros-core/)

The `ferros-core` crate is the heart of Ferros. It provides platform-agnostic debugging primitives through the `Debugger` trait and platform-specific implementations.

#### Module Structure

```
ferros-core/src/
├── lib.rs              # Public API and re-exports
├── debugger.rs         # Debugger trait definition
├── error.rs            # Error types
├── events.rs           # Event system for async notifications
├── breakpoints/        # Breakpoint management
│   ├── mod.rs
│   └── builder.rs
├── symbols/            # Symbol resolution and DWARF parsing
│   ├── mod.rs
│   ├── cache.rs
│   ├── demangle.rs
│   ├── extractor.rs
│   ├── image.rs
│   └── unwind.rs
├── types/              # Core type definitions
│   ├── mod.rs
│   ├── address.rs
│   ├── process.rs
│   ├── registers.rs
│   ├── stack.rs
│   └── symbols.rs
└── platform/           # Platform-specific implementations
    ├── mod.rs
    └── macos/
        ├── mod.rs
        ├── task.rs      # MacOSDebugger implementation
        ├── breakpoints.rs
        ├── memory.rs
        ├── threads.rs
        ├── registers/
        │   ├── arm64.rs
        │   └── x86_64.rs
        └── ...
```

#### The Debugger Trait

The `Debugger` trait is the central abstraction in Ferros. It defines what operations a debugger can perform, regardless of the underlying platform. This trait is defined in [`crates/ferros-core/src/debugger.rs`](../../crates/ferros-core/src/debugger.rs).

**Key Methods**:

- **Lifecycle**: `launch()`, `attach()`, `detach()`
- **Execution Control**: `suspend()`, `resume()`
- **Inspection**: `read_registers()`, `read_memory()`, `get_memory_regions()`
- **Thread Management**: `threads()`, `set_active_thread()`, `refresh_threads()`
- **Breakpoints**: `add_breakpoint()`, `remove_breakpoint()`, `enable_breakpoint()`, etc.
- **Stack Unwinding**: `stack_trace()`
- **State Queries**: `is_attached()`, `is_stopped()`, `stop_reason()`, `architecture()`

**Code Reference**: [`crates/ferros-core/src/debugger.rs:52-743`](../../crates/ferros-core/src/debugger.rs#L52-L743)

#### Platform Factory

The `create_debugger()` function automatically creates the correct debugger implementation for the current platform:

```rust
pub fn create_debugger() -> Result<Box<dyn Debugger>>
```

**Code Reference**: [`crates/ferros-core/src/debugger.rs:774-788`](../../crates/ferros-core/src/debugger.rs#L774-L788)

#### Error Handling

All debugger operations return `Result<T, DebuggerError>`. The error types are defined in [`crates/ferros-core/src/error.rs`](../../crates/ferros-core/src/error.rs) and use `thiserror` for structured error handling.

**Common Error Types**:
- `NotAttached`: Operation requires an attached process
- `AttachFailed`: Failed to attach to process
- `PermissionDenied`: Insufficient permissions
- `InvalidArgument`: Invalid parameter
- `Io`: I/O error (file operations, memory access, etc.)

#### Event System

The event system allows debugger backends to notify higher layers about asynchronous state changes (e.g., process stopped, breakpoint hit) without polling. Events are defined in [`crates/ferros-core/src/events.rs`](../../crates/ferros-core/src/events.rs).

**Event Types**:
- `TargetStopped { reason, thread }`: Process stopped for a reason
- `TargetResumed`: Process resumed execution

**Code Reference**: [`crates/ferros-core/src/events.rs:14-27`](../../crates/ferros-core/src/events.rs#L14-L27)

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

**Code Reference**: [`crates/ferros-core/src/breakpoints/mod.rs`](../../crates/ferros-core/src/breakpoints/mod.rs)

#### Symbol Resolution

The `symbols` module provides symbol resolution, DWARF parsing, and type introspection. It uses the `gimli` crate to parse DWARF debugging information.

**Key Components**:
- **`SymbolCache`**: Caches symbol information for binary images
- **`Symbolication`**: Resolves addresses to function names and source locations
- **`TypeExtractor`**: Extracts type information from DWARF
- **`BinaryImage`**: Represents a loaded binary and its DWARF sections

**Code Reference**: [`crates/ferros-core/src/symbols/mod.rs`](../../crates/ferros-core/src/symbols/mod.rs)

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

**Code Reference**: [`crates/ferros-core/src/types/mod.rs`](../../crates/ferros-core/src/types/mod.rs)

#### Platform-Specific Implementation (macOS)

The macOS implementation uses Mach APIs to interact with processes. The main implementation is in [`crates/ferros-core/src/platform/macos/task.rs`](../../crates/ferros-core/src/platform/macos/task.rs).

**Key Mach APIs Used**:
- `task_for_pid()`: Get Mach port to a process
- `task_threads()`: Enumerate threads
- `thread_get_state()`: Read thread registers
- `vm_read()`/`vm_write()`: Read/write memory
- `mach_vm_region()`: Enumerate memory regions
- `posix_spawn()`: Launch processes

**Architecture Support**:
- **ARM64**: Uses `ARM_THREAD_STATE64` flavor
- **x86_64**: Uses `X86_THREAD_STATE64` flavor

**Code Reference**: [`crates/ferros-core/src/platform/macos/mod.rs`](../../crates/ferros-core/src/platform/macos/mod.rs)

---

### ferros-ui

**Location**: [`crates/ferros-ui/`](../../crates/ferros-ui/)

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

The `App` struct in [`crates/ferros-ui/src/app.rs`](../../crates/ferros-ui/src/app.rs) manages the application state:

- **Debugger instance**: The `Debugger` trait object
- **View modes**: Overview, Registers, Threads, Memory Regions, Output
- **Table states**: For navigating registers, threads, memory regions
- **Process output**: Captured stdout/stderr from launched processes
- **Stop events**: Log of stop/resume events

**Code Reference**: [`crates/ferros-ui/src/app.rs:33-67`](../../crates/ferros-ui/src/app.rs#L33-L67)

#### View Modes

The TUI supports multiple view modes:

1. **Overview**: General debugger information and status
2. **Registers**: CPU register values
3. **Threads**: List of threads in the process
4. **Memory Regions**: Memory map of the process
5. **Output**: Process stdout/stderr output

**Code Reference**: [`crates/ferros-ui/src/app.rs:70-83`](../../crates/ferros-ui/src/app.rs#L70-L83)

#### Event Loop

The TUI event loop in [`crates/ferros-ui/src/tui.rs`](../../crates/ferros-ui/src/tui.rs) handles:

- **Keyboard input**: User commands (suspend, resume, navigate, quit)
- **Terminal rendering**: Updates the display on each tick
- **Background tasks**: Reading process output, forwarding debugger events
- **Cleanup**: Detaching from process and restoring terminal state

**Key Features**:
- Async event handling with Tokio
- Non-blocking process output reading
- Debugger event forwarding
- Graceful shutdown with cleanup

**Code Reference**: [`crates/ferros-ui/src/tui.rs:70-172`](../../crates/ferros-ui/src/tui.rs#L70-L172)

#### Keyboard Commands

- **`q`/`Esc`**: Quit
- **`1-5`**: Switch view modes
- **`s`**: Suspend process
- **`r`**: Resume process
- **`↑`/`↓`**: Navigate tables
- **`Ctrl+A`**: Attach to process (future)

**Code Reference**: [`crates/ferros-ui/src/app.rs:175-236`](../../crates/ferros-ui/src/app.rs#L175-L236)

---

### ferros-protocol

**Location**: [`crates/ferros-protocol/`](../../crates/ferros-protocol/)

The `ferros-protocol` crate is currently a placeholder for future remote debugging capabilities. It will define structured messages for communication between the debugger and frontends (TUI, GUI, or remote clients).

**Future Design**:
- JSON or binary (MessagePack) message format
- Protocol versioning for backward compatibility
- Support for remote debugging over network

**Code Reference**: [`crates/ferros-protocol/src/lib.rs`](../../crates/ferros-protocol/src/lib.rs)

---

### ferros-mir

**Location**: [`crates/ferros-mir/`](../../crates/ferros-mir/)

The `ferros-mir` crate is currently a placeholder for MIR (Mid-level Intermediate Representation) integration. This will enable debugging at the Rust compiler's intermediate representation level.

**Future Design**:
- MIR block visualization
- Symbolic execution of MIR
- Type and lifetime introspection
- Integration with `rustc` internals

**Code Reference**: [`crates/ferros-mir/src/lib.rs`](../../crates/ferros-mir/src/lib.rs)

---

### ferros-utils

**Location**: [`crates/ferros-utils/`](../../crates/ferros-utils/)

The `ferros-utils` crate provides shared utilities used across the workspace, primarily logging infrastructure.

#### Logging Module

The logging module in [`crates/ferros-utils/src/logging.rs`](../../crates/ferros-utils/src/logging.rs) provides structured logging using `tracing`:

**Features**:
- Multiple output formats (pretty, JSON)
- Environment variable configuration (`RUST_LOG`, `FERROS_LOG_FORMAT`)
- File and console logging
- TUI mode file-only logging (prevents stdout interference)

**Key Functions**:
- `init_logging()`: Initialize with environment variables
- `init_logging_with_level()`: Initialize with explicit level/format
- `init_logging_for_tui()`: Initialize file-only logging for TUI

**Code Reference**: [`crates/ferros-utils/src/logging.rs:152-224`](../../crates/ferros-utils/src/logging.rs#L152-L224)

---

## Data Flow

### Launching a Process

1. **User invokes**: `ferros launch ./target/debug/my_program`
2. **CLI parses command**: [`crates/ferros/src/main.rs:182-223`](../../crates/ferros/src/main.rs#L182-L223)
3. **Create debugger**: `create_debugger()` returns platform-specific implementation
4. **Launch process**: `debugger.launch()` spawns process in suspended state
5. **Resume process**: `debugger.resume()` starts execution
6. **Start TUI**: `ferros_ui::run_tui()` initializes TUI and enters event loop

### Attaching to a Process

1. **User invokes**: `ferros attach <pid>`
2. **CLI parses command**: [`crates/ferros/src/main.rs:167-180`](../../crates/ferros/src/main.rs#L167-L180)
3. **Create debugger**: Platform-specific debugger instance
4. **Attach to process**: `debugger.attach(pid)` gets Mach port (macOS) or uses ptrace (Linux)
5. **Start TUI**: Interactive debugging session begins

### Reading Registers

1. **User presses key** in TUI to view registers
2. **TUI calls**: `debugger.read_registers()`
3. **Platform implementation**: 
   - macOS: `thread_get_state()` with appropriate flavor
   - Linux: `ptrace(PTRACE_GETREGS)`
4. **Return registers**: `Registers` struct with all CPU registers
5. **TUI renders**: Register values in a table

### Breakpoint Flow

1. **User requests breakpoint**: Via CLI or TUI
2. **Create request**: `BreakpointRequest::Software { address }`
3. **Add breakpoint**: `debugger.add_breakpoint(request)`
4. **Platform installs**:
   - macOS: Read original instruction, write BRK/INT3, store original bytes
   - Linux: Similar process with ptrace
5. **Process hits breakpoint**: Kernel delivers exception
6. **Debugger receives event**: `TargetStopped { reason: Breakpoint(addr) }`
7. **TUI updates**: Shows breakpoint hit, allows inspection

---

## Platform Abstraction

Ferros uses a trait-based approach to abstract platform differences. The `Debugger` trait defines the interface, and each platform provides its own implementation.

### macOS Implementation

**Location**: [`crates/ferros-core/src/platform/macos/`](../../crates/ferros-core/src/platform/macos/)

**Key Files**:
- **`task.rs`**: Main `MacOSDebugger` implementation
- **`ffi.rs`**: Foreign function declarations for Mach APIs
- **`launch.rs`**: Process launching with `posix_spawn`
- **`memory.rs`**: Memory reading/writing with `vm_read`/`vm_write`
- **`threads.rs`**: Thread enumeration and management
- **`registers/`**: Architecture-specific register handling

**Code Reference**: [`crates/ferros-core/src/platform/macos/mod.rs`](../../crates/ferros-core/src/platform/macos/mod.rs)

### Future: Linux Implementation

Linux will use `ptrace` system calls:
- `PTRACE_ATTACH`: Attach to process
- `PTRACE_GETREGS`: Read registers
- `PTRACE_PEEKDATA`/`PTRACE_POKEDATA`: Read/write memory
- `PTRACE_CONT`: Resume execution

### Future: Windows Implementation

Windows will use the Windows Debug API:
- `DebugActiveProcess()`: Attach to process
- `GetThreadContext()`/`SetThreadContext()`: Register access
- `ReadProcessMemory()`/`WriteProcessMemory()`: Memory access
- `ContinueDebugEvent()`: Resume execution

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
let regs = debugger.read_registers()?;  // Propagates DebuggerError
```

**Code Reference**: [`crates/ferros-core/src/error.rs`](../../crates/ferros-core/src/error.rs)

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

Ferros is designed with a clear separation of concerns, allowing each component to be developed and tested independently. The trait-based platform abstraction enables cross-platform support while maintaining a clean API.

For contributors, the key entry points are:
- **CLI**: [`crates/ferros/src/main.rs`](../../crates/ferros/src/main.rs)
- **Core API**: [`crates/ferros-core/src/debugger.rs`](../../crates/ferros-core/src/debugger.rs)
- **Platform Implementation**: [`crates/ferros-core/src/platform/`](../../crates/ferros-core/src/platform/)
- **TUI**: [`crates/ferros-ui/src/app.rs`](../../crates/ferros-ui/src/app.rs)

Each module is well-documented with Rustdoc comments, and the codebase follows Rust best practices for safety, performance, and maintainability.

