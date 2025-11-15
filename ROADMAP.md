# 🦀 Ferros — Detailed Step-by-Step Debugger Development Roadmap

This roadmap outlines **detailed, incremental milestones** for developing **Ferros**, a Rust-native debugger from scratch. Each phase is broken down into small, achievable tasks perfect for learning as you build.

> **Philosophy**: Start small, learn fundamentals, build iteratively, test constantly.

---

## 🎯 Prerequisites & Learning Path

### Pre-Phase: Essential Knowledge
> Goal: Understand core concepts before writing debugger code

#### 1️⃣ Computer Science Fundamentals
- [x] **Task 1.1**: Study process memory layout (stack, heap, data, text segments)
  - Read: "Computer Systems: A Programmer's Perspective" Chapter 7
  - Goal: Understand how programs are organized in memory
  
- [x] **Task 1.2**: Learn about CPU registers and instruction pointer
  - Research: x86-64 registers (RAX, RBX, RIP, RSP, RBP, etc.)
  - Goal: Know what registers debuggers need to read/write
  
- [x] **Task 1.3**: Understand system calls and signals
  - Read: Linux man pages for `ptrace(2)`, `waitpid(2)`, `kill(2)`
  - Goal: Learn how processes communicate with the OS

#### 2️⃣ Debugger Theory
- [ ] **Task 2.1**: Read "How Debuggers Work" tutorial
  - Resource: https://www.debuggingbook.org/html/Debugger.html
  - Goal: Understand debugger architecture at a high level
  
- [ ] **Task 2.2**: Study GDB architecture documentation
  - Read: GDB Internals Manual (sections 1-3)
  - Goal: Learn from industry-standard debugger design
  
- [ ] **Task 2.3**: Learn about breakpoint mechanisms
  - Research: Software breakpoints (INT3), hardware breakpoints
  - Goal: Understand how to pause program execution

#### 3️⃣ Debug Information Formats
- [ ] **Task 3.1**: Read DWARF specification overview (chapter 1-2)
  - Resource: http://dwarfstd.org/doc/DWARF5.pdf
  - Goal: Understand how debug symbols are stored
  
- [ ] **Task 3.2**: Explore ELF binary format
  - Tool: Use `readelf -a <binary>` on sample programs
  - Goal: See what's inside a compiled binary
  
- [ ] **Task 3.3**: Learn about symbol tables and debug sections
  - Research: `.debug_info`, `.debug_line`, `.symtab` sections
  - Goal: Know where to find function/variable information

#### 4️⃣ Rust-Specific Knowledge
- [ ] **Task 4.1**: Study Rust's compilation pipeline
  - Read: Rust Compiler Development Guide (rustc-dev-guide)
  - Goal: Understand HIR → MIR → LLVM IR flow
  
- [ ] **Task 4.2**: Learn about Rust's debug info generation
  - Experiment: Compile with `-C debuginfo=2` and inspect output
  - Goal: See what debug data Rust generates
  
- [ ] **Task 4.3**: Research Rust's memory model
  - Topics: Ownership, borrowing, lifetimes, drop semantics
  - Goal: Know what makes Rust debugging unique

---

## 🧪 Phase 1 — Proof of Concept Projects
> Goal: Build tiny learning projects to validate concepts before building the full debugger

### Project 1.1: Hello ptrace
**Objective**: Attach to a process and read its registers

- [x] **Task 1.1.1**: Create a simple "target" program
  - Create `examples/hello_target.rs` that prints and loops
  - This will be your program to debug
  
- [x] **Task 1.1.2**: Write a program that uses `ptrace` to attach
  - Use the `nix` crate for `ptrace` bindings
  - Attach to the target process by PID
  
- [x] **Task 1.1.3**: Read and print all general-purpose registers
  - Use `PTRACE_GETREGS` to read register state
  - Print register values in hex format
  
- [x] **Task 1.1.4**: Detach cleanly from the target
  - Use `PTRACE_DETACH` to release the process
  - Verify target continues running

**Success Criteria**: You can attach to a running process and print its register values

### Project 1.2: Memory Inspector
**Objective**: Read and write process memory

- [x] **Task 1.2.1**: Extend hello_target to have known data
  - Add a variable with a known value
  - Print its address for testing
  
- [x] **Task 1.2.2**: Implement memory reading
  - Read the variable's value from another process using platform-specific APIs
  - Verify it matches the expected value
  
- [x] **Task 1.2.3**: Implement memory writing
  - Change the variable's value from the debugger
  - Verify the target sees the new value
  
- [x] **Task 1.2.4**: Implement memory region enumeration
  - Read and display the target's memory regions using platform-specific APIs
  - Identify stack, heap, and code segments

**Success Criteria**: You can read and modify another process's memory

### Project 1.3: Simple Breakpoint
**Objective**: Set a software breakpoint and handle it

- [ ] **Task 1.3.1**: Learn about INT3 instruction (0xCC on x86)
  - Research: How INT3 generates SIGTRAP
  - Goal: Understand breakpoint mechanics
  
- [ ] **Task 1.3.2**: Find an instruction address in the target
  - Use `nm` or `objdump` to find a function address
  - Note the address for breakpoint placement
  
- [ ] **Task 1.3.3**: Replace instruction with 0xCC
  - Save the original byte
  - Write 0xCC using `PTRACE_POKEDATA`
  
- [ ] **Task 1.3.4**: Continue execution and catch SIGTRAP
  - Use `waitpid()` to wait for the signal
  - Verify you stopped at the breakpoint
  
- [ ] **Task 1.3.5**: Restore original instruction
  - Write the saved byte back
  - Decrement instruction pointer (RIP) by 1

**Success Criteria**: You can set a breakpoint and pause execution when hit

### Project 1.4: Symbol Reader
**Objective**: Parse DWARF debug information

- [ ] **Task 1.4.1**: Install and explore `gimli` crate
  - Add `gimli` and `object` to dependencies
  - Read their documentation
  
- [ ] **Task 1.4.2**: Load an ELF binary
  - Use `object` crate to read a compiled binary
  - List all sections in the file
  
- [ ] **Task 1.4.3**: Parse `.debug_info` section
  - Extract debug information entries (DIEs)
  - Print all function names found
  
- [ ] **Task 1.4.4**: Parse `.debug_line` section
  - Extract line number program
  - Map addresses to source file:line numbers
  
- [ ] **Task 1.4.5**: Build a simple symbol table
  - Create a HashMap of address → symbol name
  - Test looking up function names by address

**Success Criteria**: You can parse a binary and extract function names and line numbers

### Project 1.5: Basic Stepper
**Objective**: Implement single-step execution

- [ ] **Task 1.5.1**: Use `PTRACE_SINGLESTEP`
  - Execute one instruction at a time
  - Print the instruction pointer after each step
  
- [ ] **Task 1.5.2**: Count instructions executed
  - Step through a simple function
  - Count how many instructions it takes
  
- [ ] **Task 1.5.3**: Combine stepping with symbol lookup
  - Step through code
  - Print function names as you enter/exit them

**Success Criteria**: You can step through a program instruction by instruction

---

## 🏗️ Phase 2 — Architect the Core
> Goal: Design the foundation architecture for the full debugger

### Architecture Planning
- [ ] **Task 2.1**: Review existing documentation
  - Re-read `/docs/architecture.md`
  - Update based on learnings from Phase 1
  
- [ ] **Task 2.2**: Design the trait hierarchy
  - Define `Debugger` trait (high-level interface)
  - Define `DebuggerBackend` trait (OS-specific implementations)
  - Define `SymbolProvider` trait (debug info abstraction)
  
- [ ] **Task 2.3**: Design the module structure
  - Plan crate organization (already defined in architecture.md)
  - Define public APIs for each crate
  - Document inter-crate dependencies
  
- [ ] **Task 2.4**: Choose error handling strategy
  - Use `thiserror` for library errors
  - Use `anyhow` for application errors
  - Define error types for each crate
  
- [ ] **Task 2.5**: Plan async/sync boundaries
  - Decide which operations should be async
  - Design how to handle blocking system calls
  - Choose async runtime (tokio recommended)

---

## 🧱 Phase 3 — Build ferros-core (Process Control)
> Goal: Implement low-level debugging primitives for Linux

### Setup ferros-core crate
- [ ] **Task 3.1**: Initialize the crate structure
  - Create `crates/ferros-core/src/lib.rs`
  - Set up module organization
  - Add initial dependencies (`nix`, `thiserror`, `libc`)
  
- [ ] **Task 3.2**: Define core types
  - Create `ProcessId` newtype wrapper
  - Create `Address` newtype for memory addresses
  - Create `Register` enum for CPU registers
  - Document each type with examples

### Implement Process Management
- [ ] **Task 3.3**: Create `Process` struct
  - Store PID and process state
  - Implement `Debug` and `Display` traits
  
- [ ] **Task 3.4**: Implement process launching
  - Create `Process::launch(command: &str)` method
  - Fork and exec the target program
  - Trace the child with `PTRACE_TRACEME`
  - Return `Process` handle
  
- [ ] **Task 3.5**: Implement process attachment
  - Create `Process::attach(pid: ProcessId)` method
  - Use `PTRACE_ATTACH`
  - Wait for the process to stop
  - Return `Process` handle
  
- [ ] **Task 3.6**: Implement process detachment
  - Create `Process::detach()` method
  - Use `PTRACE_DETACH`
  - Handle cleanup and error cases

### Implement Register Access
- [ ] **Task 3.7**: Create `Registers` struct
  - Define fields for all general-purpose registers
  - Implement getters for common registers (RIP, RSP, etc.)
  
- [ ] **Task 3.8**: Implement `read_registers()`
  - Use `PTRACE_GETREGS`
  - Parse register struct from kernel
  - Return `Registers` object
  
- [ ] **Task 3.9**: Implement `write_registers()`
  - Take a `Registers` object
  - Use `PTRACE_SETREGS`
  - Handle partial updates

### Implement Memory Access
- [ ] **Task 3.10**: Create `Memory` interface
  - Define `read_memory(addr, size)` method
  - Define `write_memory(addr, data)` method
  
- [ ] **Task 3.11**: Implement memory reading
  - Use `PTRACE_PEEKDATA` in a loop
  - Handle word-aligned reads
  - Support arbitrary sizes
  
- [ ] **Task 3.12**: Implement memory writing
  - Use `PTRACE_POKEDATA` in a loop
  - Preserve unmodified bytes in partial words
  - Return bytes written
  
- [ ] **Task 3.13**: Implement memory region parsing
  - Parse `/proc/[pid]/maps`
  - Create `MemoryRegion` struct (start, end, perms, name)
  - Return list of all regions

### Implement Breakpoint Support
- [ ] **Task 3.14**: Create `Breakpoint` struct
  - Store address, original byte, enabled state
  - Implement `Debug` trait
  
- [ ] **Task 3.15**: Create `BreakpointManager`
  - Store map of address → Breakpoint
  - Provide add/remove/enable/disable methods
  
- [ ] **Task 3.16**: Implement breakpoint setting
  - Read original instruction byte
  - Write 0xCC (INT3) to address
  - Store original byte for restoration
  
- [ ] **Task 3.17**: Implement breakpoint removal
  - Restore original byte
  - Remove from internal map
  
- [ ] **Task 3.18**: Implement breakpoint hit handling
  - Detect SIGTRAP from breakpoint
  - Restore original instruction
  - Rewind instruction pointer (RIP -= 1)
  - Prepare for re-execution or stepping

### Implement Execution Control
- [ ] **Task 3.19**: Implement `continue_execution()`
  - Use `PTRACE_CONT`
  - Wait for next stop event
  - Return stop reason (signal, breakpoint, exit)
  
- [ ] **Task 3.20**: Implement `single_step()`
  - Use `PTRACE_SINGLESTEP`
  - Execute one instruction
  - Return updated instruction pointer
  
- [ ] **Task 3.21**: Implement `step_over()` (step next)
  - Detect if current instruction is a call
  - Set temporary breakpoint after call
  - Continue to that breakpoint
  
- [ ] **Task 3.22**: Implement `step_out()` (finish)
  - Get return address from stack frame
  - Set temporary breakpoint at return address
  - Continue until hit

### Handle Signals and Events
- [ ] **Task 3.23**: Create `StopReason` enum
  - Variants: Breakpoint, Signal, Exited, Terminated
  - Store associated data (signal number, exit code, etc.)
  
- [ ] **Task 3.24**: Implement signal handling
  - Use `waitpid()` with status parsing
  - Distinguish SIGTRAP from other signals
  - Forward non-debug signals to target
  
- [ ] **Task 3.25**: Implement process exit detection
  - Handle `WIFEXITED` and `WIFSIGNALED`
  - Clean up resources
  - Return exit status

### Add Tests
- [ ] **Task 3.26**: Write unit tests for memory operations
  - Test reading/writing various sizes
  - Test boundary conditions
  
- [ ] **Task 3.27**: Write integration tests for breakpoints
  - Create test binaries in `tests/fixtures/`
  - Test setting and hitting breakpoints
  
- [ ] **Task 3.28**: Write tests for stepping
  - Test single-step through known code
  - Verify instruction counts
  
- [ ] **Task 3.29**: Add error handling tests
  - Test attaching to non-existent PIDs
  - Test invalid memory addresses
  - Verify proper error messages

---

## 🔍 Phase 4 — Build ferros-utils (Shared Utilities)
> Goal: Create common utilities used across all crates

### Setup Infrastructure
- [ ] **Task 4.1**: Initialize ferros-utils crate
  - Create basic structure
  - Add `tracing` and `serde` dependencies
  
- [ ] **Task 4.2**: Create logging infrastructure
  - Set up `tracing` subscriber configuration
  - Add log level filtering
  - Create convenience macros for debug logging

### Build Common Types
- [ ] **Task 4.3**: Create `Result` type alias
  - Define project-wide result type
  - Create convenience error types
  
- [ ] **Task 4.4**: Create configuration types
  - Define `DebuggerConfig` struct
  - Support loading from TOML/JSON
  - Add environment variable overrides
  
- [ ] **Task 4.5**: Create platform detection
  - Detect OS (Linux, macOS, Windows)
  - Detect architecture (x86_64, ARM, etc.)
  - Provide compile-time and runtime checks

### Add Utilities
- [ ] **Task 4.6**: Create address formatting utilities
  - Format addresses as hex with proper padding
  - Create byte array display helpers
  
- [ ] **Task 4.7**: Create disassembler integration (optional)
  - Integrate `capstone` or `iced-x86`
  - Provide instruction disassembly helpers
  
- [ ] **Task 4.8**: Add documentation
  - Document all public APIs
  - Add usage examples

---

## 📚 Phase 5 — Build ferros-protocol (Symbol System)
> Goal: Parse and manage debug symbols and type information

### Setup Symbol Parsing
- [ ] **Task 5.1**: Initialize ferros-protocol crate
  - Add `gimli`, `object`, `addr2line` dependencies
  - Set up module structure
  
- [ ] **Task 5.2**: Create `Binary` abstraction
  - Load ELF/Mach-O/PE files using `object` crate
  - Extract relevant sections
  - Provide section query interface

### Implement DWARF Parsing
- [ ] **Task 5.3**: Create `DwarfParser` struct
  - Initialize gimli's `Dwarf` struct
  - Provide iteration over compilation units
  
- [ ] **Task 5.4**: Extract function information
  - Parse function DIEs (Debug Information Entries)
  - Extract name, address range, source file
  - Build function name → address mapping
  
- [ ] **Task 5.5**: Extract line number information
  - Parse `.debug_line` section
  - Build address → source location mapping
  - Support reverse lookup (source line → address)
  
- [ ] **Task 5.6**: Extract variable information
  - Parse local and global variable DIEs
  - Extract name, type, location expressions
  - Handle DWARF location lists

### Implement Type System
- [ ] **Task 5.7**: Create `TypeInfo` enum
  - Variants: Primitive, Struct, Enum, Array, Pointer, etc.
  - Store size, alignment, fields
  
- [ ] **Task 5.8**: Parse type information from DWARF
  - Build type graph from type DIEs
  - Resolve type references
  - Handle recursive types
  
- [ ] **Task 5.9**: Implement pretty-printing for types
  - Format types in Rust syntax
  - Show struct fields, enum variants
  - Handle nested types

### Build Symbol Table
- [ ] **Task 5.10**: Create `SymbolTable` struct
  - Store functions, variables, types
  - Provide fast lookup by address or name
  - Support incremental updates
  
- [ ] **Task 5.11**: Implement symbol caching
  - Cache parsed symbols to disk
  - Invalidate on binary changes
  - Load cache on startup for performance
  
- [ ] **Task 5.12**: Add symbol name demangling
  - Use `rustc-demangle` for Rust symbols
  - Support C++ demangling as well
  - Make human-readable symbol names

### Implement Source Code Mapping
- [ ] **Task 5.13**: Create `SourceMap` struct
  - Map addresses to file:line:column
  - Map source locations to address ranges
  
- [ ] **Task 5.14**: Implement source file reading
  - Load source files referenced in debug info
  - Cache file contents
  - Handle missing files gracefully
  
- [ ] **Task 5.15**: Implement inlined function support
  - Parse DWARF inline info
  - Track inlining depth
  - Show full inline call stack

### Add Tests
- [ ] **Task 5.16**: Create test binaries with known debug info
  - Compile simple programs with `-C debuginfo=2`
  - Document expected symbols
  
- [ ] **Task 5.17**: Write tests for symbol lookup
  - Test function name lookup
  - Test address-to-line mapping
  - Test type parsing
  
- [ ] **Task 5.18**: Test with optimized code
  - Compile with `-O2` or `-O3`
  - Verify debug info still works
  - Document limitations

---

## 🎨 Phase 6 — Build ferros CLI (Command-Line Interface)
> Goal: Create a user-friendly command-line debugger

### Setup CLI Infrastructure
- [ ] **Task 6.1**: Initialize ferros crate (binary)
  - Set up main.rs
  - Add `clap` for argument parsing
  - Add `rustyline` for interactive input
  
- [ ] **Task 6.2**: Design command structure
  - Define top-level commands (run, attach, etc.)
  - Define subcommands and options
  - Create help text

### Implement Debugger Session
- [ ] **Task 6.3**: Create `Session` struct
  - Store process, symbols, breakpoints
  - Track current state (running, stopped, etc.)
  - Manage user preferences
  
- [ ] **Task 6.4**: Implement `run` command
  - Parse arguments
  - Launch target process using ferros-core
  - Load symbols using ferros-protocol
  - Enter interactive mode
  
- [ ] **Task 6.5**: Implement `attach` command
  - Parse PID from arguments
  - Attach to running process
  - Load symbols for the binary
  - Enter interactive mode

### Build Interactive REPL
- [ ] **Task 6.6**: Create command parser
  - Parse user input into commands
  - Support command shortcuts (b for break, c for continue)
  - Handle arguments and options
  
- [ ] **Task 6.7**: Implement `break` command
  - Parse breakpoint location (address, function, file:line)
  - Resolve location to address using symbols
  - Set breakpoint using ferros-core
  - Display confirmation
  
- [ ] **Task 6.8**: Implement `continue` command
  - Resume process execution
  - Wait for next stop event
  - Display stop reason and location
  
- [ ] **Task 6.9**: Implement `step` command (step into)
  - Single-step instruction
  - Show new location with source context
  
- [ ] **Task 6.10**: Implement `next` command (step over)
  - Step over function calls
  - Show new location
  
- [ ] **Task 6.11**: Implement `finish` command (step out)
  - Run until function returns
  - Show return location

### Implement Information Commands
- [ ] **Task 6.12**: Implement `info` command group
  - `info breakpoints` - list all breakpoints
  - `info registers` - show register values
  - `info locals` - show local variables (basic)
  - `info threads` - show threads (placeholder for later)
  
- [ ] **Task 6.13**: Implement `backtrace` (bt) command
  - Walk stack frames
  - Show function names and locations
  - Display frame numbers
  
- [ ] **Task 6.14**: Implement `frame` command
  - Select a specific stack frame
  - Show frame details
  
- [ ] **Task 6.15**: Implement `list` command
  - Show source code around current location
  - Support line ranges
  - Highlight current line

### Implement Memory Inspection
- [ ] **Task 6.16**: Implement `print` command
  - Read memory at address
  - Format as different types (hex, int, string)
  - Support expressions (basic)
  
- [ ] **Task 6.17**: Implement `examine` (x) command
  - Dump memory in various formats
  - Support format specifiers (like gdb)
  
- [ ] **Task 6.18**: Implement `disassemble` command
  - Disassemble instructions around location
  - Show current instruction pointer
  - Highlight upcoming instructions

### Add Breakpoint Management
- [ ] **Task 6.19**: Implement `delete` command
  - Delete breakpoint by number
  - Confirm deletion
  
- [ ] **Task 6.20**: Implement `enable/disable` commands
  - Toggle breakpoints without deleting
  - Show status changes
  
- [ ] **Task 6.21**: Implement conditional breakpoints
  - Parse condition expressions
  - Evaluate when hit
  - Continue if condition false

### Polish User Experience
- [ ] **Task 6.22**: Add colored output
  - Highlight keywords, addresses, errors
  - Use `colored` or `termcolor` crate
  
- [ ] **Task 6.23**: Implement command history
  - Save across sessions using rustyline
  - Support history search
  
- [ ] **Task 6.24**: Add tab completion
  - Complete command names
  - Complete function names
  - Complete file names
  
- [ ] **Task 6.25**: Implement help system
  - `help` command shows all commands
  - `help <command>` shows detailed help
  - Include examples

### Add Error Handling
- [ ] **Task 6.26**: Implement graceful error messages
  - Catch and display errors clearly
  - Suggest fixes when possible
  
- [ ] **Task 6.27**: Handle Ctrl-C gracefully
  - Don't crash debugger
  - Interrupt target process instead
  
- [ ] **Task 6.28**: Add debug mode
  - `--debug` flag for verbose logging
  - Log all ptrace calls
  - Useful for debugging the debugger

---

## 🧬 Phase 7 — Advanced Features (Stack Unwinding)
> Goal: Implement proper stack walking and frame inspection

### Implement Stack Unwinding
- [ ] **Task 7.1**: Study DWARF CFI (Call Frame Information)
  - Read about `.debug_frame` and `.eh_frame`
  - Understand how to reconstruct stack frames
  
- [ ] **Task 7.2**: Parse CFI data
  - Use gimli to parse CFI
  - Understand CFA (Canonical Frame Address)
  - Handle register save locations
  
- [ ] **Task 7.3**: Implement frame unwinder
  - Walk stack using CFI rules
  - Calculate frame addresses
  - Restore registers at each frame
  
- [ ] **Task 7.4**: Build `StackFrame` type
  - Store frame address, return address
  - Store saved register values
  - Link to function symbol
  
- [ ] **Task 7.5**: Implement `backtrace()` method
  - Unwind full call stack
  - Handle errors gracefully
  - Support showing frame details

### Implement Variable Inspection
- [ ] **Task 7.6**: Parse DWARF location expressions
  - Understand location lists
  - Handle register-relative addresses
  - Handle TLS (thread-local storage)
  
- [ ] **Task 7.7**: Evaluate variable locations
  - Compute variable addresses from DWARF
  - Read memory at computed locations
  - Handle variables in registers
  
- [ ] **Task 7.8**: Format variable values
  - Pretty-print primitive types
  - Show struct fields
  - Handle pointers and references
  
- [ ] **Task 7.9**: Implement recursive pretty-printing
  - Handle nested structures
  - Limit recursion depth
  - Show type information

---

## 🧠 Phase 8 — MIR Integration (Advanced)
> Goal: Integrate Rust compiler internals for MIR-level debugging

### Setup MIR Infrastructure
- [ ] **Task 8.1**: Research `rustc_private` usage
  - Understand stability caveats
  - Set up nightly toolchain requirement
  - Read rustc-dev-guide chapters on MIR
  
- [ ] **Task 8.2**: Initialize ferros-mir crate
  - Set up with nightly toolchain
  - Add rustc_private dependencies
  - Create basic structure
  
- [ ] **Task 8.3**: Load MIR for a crate
  - Use `rustc_interface` to compile and extract MIR
  - Store MIR for all functions
  - Map MIR to machine code addresses

### Implement MIR Interpreter
- [ ] **Task 8.4**: Study Miri's architecture
  - Read Miri source code
  - Understand evaluation context
  - Learn from their interpreter design
  
- [ ] **Task 8.5**: Create MIR evaluation engine (basic)
  - Evaluate arithmetic operations
  - Handle control flow (basic blocks)
  - Track variable values
  
- [ ] **Task 8.6**: Implement symbolic execution (optional)
  - Track multiple possible values
  - Build path constraints
  - Identify branches not taken

### Visualize Ownership
- [ ] **Task 8.7**: Track borrow states
  - Identify borrows from MIR
  - Track lifetime relationships
  - Visualize active borrows
  
- [ ] **Task 8.8**: Visualize move semantics
  - Identify when values are moved
  - Show moved-from variables
  - Highlight use-after-move errors

### Mixed-Mode Debugging
- [ ] **Task 8.9**: Map native execution to MIR
  - Correlate machine code positions to MIR
  - Switch between native and MIR views
  
- [ ] **Task 8.10**: Implement MIR-level breakpoints
  - Set breakpoints at MIR statement boundaries
  - Map to machine code addresses

---

## 🧪 Phase 9 — Comprehensive Testing
> Goal: Ensure debugger reliability and correctness

### Create Test Suite
- [ ] **Task 9.1**: Build test binary collection
  - Simple programs (hello world, loops, recursion)
  - Complex programs (async, threads, panics)
  - Optimized builds (-O2, -O3)
  
- [ ] **Task 9.2**: Write integration tests
  - Test launching programs
  - Test attaching to processes
  - Test all CLI commands
  
- [ ] **Task 9.3**: Write regression tests
  - Document known issues
  - Create tests that would catch them
  - Verify fixes don't regress

### Test Edge Cases
- [ ] **Task 9.4**: Test with stripped binaries
  - Binaries without debug info
  - Verify graceful degradation
  
- [ ] **Task 9.5**: Test with PIE (Position Independent Executables)
  - Handle ASLR correctly
  - Test address calculations
  
- [ ] **Task 9.6**: Test multithreaded programs
  - Attach to threaded programs
  - Handle per-thread state
  
- [ ] **Task 9.7**: Test signal handling
  - Programs that use signals
  - Verify signals forwarded correctly

### Performance Testing
- [ ] **Task 9.8**: Benchmark symbol loading
  - Test with large binaries
  - Measure cache effectiveness
  
- [ ] **Task 9.9**: Benchmark stepping performance
  - Measure single-step overhead
  - Profile and optimize hot paths

---

## 📖 Phase 10 — Documentation
> Goal: Create comprehensive documentation for users and contributors

### User Documentation
- [ ] **Task 10.1**: Write getting started guide
  - Installation instructions
  - First debugging session tutorial
  - Common commands reference
  
- [ ] **Task 10.2**: Write command reference
  - Document all commands
  - Include examples
  - Show expected output
  
- [ ] **Task 10.3**: Write troubleshooting guide
  - Common issues and solutions
  - Debug mode usage
  - How to report bugs

### Developer Documentation
- [ ] **Task 10.4**: Document architecture
  - Update `docs/architecture.md`
  - Add architecture diagrams
  - Explain design decisions
  
- [ ] **Task 10.5**: Write contributor guide
  - Development setup
  - Code style guidelines
  - Testing requirements
  - PR process
  
- [ ] **Task 10.6**: Document internal APIs
  - Rustdoc for all public APIs
  - Add module-level documentation
  - Include examples in docs
  
- [ ] **Task 10.7**: Write debugging guide for Ferros
  - How to debug the debugger
  - Common pitfalls
  - Useful tools

### Create Examples
- [ ] **Task 10.8**: Write tutorial examples
  - Example debugging sessions
  - Annotated transcripts
  - Explain what's happening at each step
  
- [ ] **Task 10.9**: Create video tutorial (optional)
  - Record debugging session
  - Explain features
  - Post to YouTube

---

## 🎯 Phase 11 — IDE Integration (DAP)
> Goal: Support IDE debugging through Debug Adapter Protocol

### Research DAP
- [ ] **Task 11.1**: Study DAP specification
  - Read Microsoft's DAP documentation
  - Understand message format (JSON-RPC)
  - Learn about capabilities negotiation
  
- [ ] **Task 11.2**: Study existing DAP implementations
  - Look at rust-analyzer's DAP support
  - Study lldb-vscode adapter
  - Identify patterns to follow

### Implement DAP Server
- [ ] **Task 11.3**: Create ferros-dap crate
  - Set up JSON-RPC handling
  - Use `serde_json` for message parsing
  
- [ ] **Task 11.4**: Implement DAP initialization
  - Handle `initialize` request
  - Respond with capabilities
  - Handle `launch` and `attach` requests
  
- [ ] **Task 11.5**: Implement breakpoint requests
  - Handle `setBreakpoints` request
  - Map source locations to addresses
  - Return confirmed breakpoints
  
- [ ] **Task 11.6**: Implement execution control
  - Handle `continue`, `next`, `stepIn`, `stepOut`
  - Send `stopped` events
  - Handle pause requests
  
- [ ] **Task 11.7**: Implement stack trace requests
  - Handle `stackTrace` request
  - Return formatted stack frames
  - Include source locations
  
- [ ] **Task 11.8**: Implement variable inspection
  - Handle `scopes` request
  - Handle `variables` request
  - Support variable children (struct fields)

### Create VS Code Extension
- [ ] **Task 11.9**: Initialize VS Code extension
  - Create `package.json`
  - Define debugger contribution
  - Specify launch configurations
  
- [ ] **Task 11.10**: Implement extension activation
  - Register debug adapter
  - Provide launch templates
  
- [ ] **Task 11.11**: Test in VS Code
  - Load extension in development mode
  - Test all debugging features
  - Fix any integration issues
  
- [ ] **Task 11.12**: Publish extension
  - Package as VSIX
  - Publish to VS Code marketplace
  - Document installation process

---

## 🚀 Phase 12 — Release Preparation
> Goal: Prepare for first public release

### Code Quality
- [ ] **Task 12.1**: Run full lint and format pass
  - `cargo fmt --all`
  - `cargo clippy --all -- -D warnings`
  - Fix all warnings
  
- [ ] **Task 12.2**: Review all error messages
  - Ensure they're helpful
  - Add suggestions where appropriate
  - Test error paths
  
- [ ] **Task 12.3**: Add `#![deny(unsafe_code)]` where appropriate
  - Verify unsafe code is minimal and documented
  - Add `# Safety` documentation

### Performance & Optimization
- [ ] **Task 12.4**: Profile the debugger
  - Use `perf` or `cargo flamegraph`
  - Identify hot paths
  
- [ ] **Task 12.5**: Optimize symbol loading
  - Lazy-load symbols when possible
  - Improve cache hit rates
  
- [ ] **Task 12.6**: Optimize stepping performance
  - Minimize syscalls
  - Batch operations when possible

### Packaging
- [ ] **Task 12.7**: Set up CI/CD
  - GitHub Actions for testing
  - Test on Linux, macOS (Windows later)
  - Run tests on every PR
  
- [ ] **Task 12.8**: Create release builds
  - Build optimized binaries
  - Strip debug symbols from release
  - Test release builds
  
- [ ] **Task 12.9**: Prepare crates.io release
  - Update version numbers
  - Write changelogs
  - Verify metadata
  
- [ ] **Task 12.10**: Create GitHub release
  - Tag version
  - Write release notes
  - Attach binaries

### Marketing & Community
- [ ] **Task 12.11**: Write announcement blog post
  - Explain what Ferros is
  - Show cool features
  - Provide getting started instructions
  
- [ ] **Task 12.12**: Create demo video/GIF
  - Show debugging in action
  - Highlight unique features
  - Keep it short (< 2 minutes)
  
- [ ] **Task 12.13**: Announce on social media
  - Post to r/rust
  - Tweet announcement
  - Post in Rust Discord
  - Post in This Week in Rust
  
- [ ] **Task 12.14**: Set up issue templates
  - Bug report template
  - Feature request template
  - Question template

---

## 🔮 Phase 13 — Future Enhancements
> Goal: Advanced features for future releases

### Multi-Platform Support
- [ ] **Task 13.1**: Implement macOS support (Mach ports)
- [ ] **Task 13.2**: Implement Windows support (Debug API)
- [ ] **Task 13.3**: Test on ARM64 architecture

### Advanced Features
- [ ] **Task 13.4**: Implement reverse debugging (like rr)
- [ ] **Task 13.5**: Add time-travel debugging
- [ ] **Task 13.6**: Implement watchpoints (data breakpoints)
- [ ] **Task 13.7**: Add conditional expression evaluation
- [ ] **Task 13.8**: Support remote debugging

### UI Improvements
- [ ] **Task 13.9**: Build TUI with ratatui
  - Multi-panel interface
  - Source view, variables, stack, console
- [ ] **Task 13.10**: Build GUI with egui (optional)
  - Modern graphical interface
  - Memory visualizations

### Rust-Specific Features
- [ ] **Task 13.11**: Visualize async state machines
- [ ] **Task 13.12**: Show Future poll states
- [ ] **Task 13.13**: Implement cargo integration (cargo-ferros)
- [ ] **Task 13.14**: Add panic inspection tools
- [ ] **Task 13.15**: Detect common Rust bugs (borrow errors, etc.)

---

## 📊 Milestone Tracking

### Milestone 1: POC Complete (Proof of Concepts Done)
- All Phase 1 projects working
- Can attach, read memory, set breakpoints, step

### Milestone 2: Core Complete
- ferros-core fully implemented
- ferros-utils implemented
- All tests passing

### Milestone 3: Symbols Complete
- ferros-protocol implemented
- Can parse DWARF and resolve symbols
- Source code mapping working

### Milestone 4: CLI Complete
- ferros CLI implemented
- All basic commands working
- Good user experience

### Milestone 5: Advanced Features Complete
- Stack unwinding working
- Variable inspection working
- Frame navigation working

### Milestone 6: v0.1.0 Release
- All documentation complete
- Tests passing
- Released on crates.io

### Milestone 7: IDE Integration
- DAP server complete
- VS Code extension working
- Published to marketplace

---

## 📝 Daily Development Tips

### How to Stay Motivated
1. **Work on one task at a time** - Don't jump ahead
2. **Celebrate small wins** - Each checkbox is progress!
3. **Document as you go** - Future you will thank you
4. **Test frequently** - Catch bugs early
5. **Take breaks** - Complex systems need fresh eyes

### Recommended Development Cycle
1. Pick a task from the roadmap
2. Research if needed (read docs, look at examples)
3. Write a failing test first (TDD)
4. Implement the feature
5. Make the test pass
6. Refactor and clean up
7. Document what you learned
8. Commit with good message
9. Take a break!

### Learning Resources
- **Rust Compiler Dev Guide**: https://rustc-dev-guide.rust-lang.org
- **DWARF Standard**: http://dwarfstd.org/
- **Linux ptrace man page**: `man 2 ptrace`
- **gimli documentation**: https://docs.rs/gimli
- **Debugging Book**: https://www.debuggingbook.org/

---

## 🎉 Success Criteria

You'll know you've succeeded when:
- [ ] You can debug a simple Rust program from the CLI
- [ ] You can set breakpoints and inspect variables
- [ ] The debugger helps you find real bugs
- [ ] Other people can use and understand it
- [ ] You deeply understand how debuggers work
- [ ] You're proud to show it off!

---

> **Remember**: Building a debugger is a marathon, not a sprint. Take it one task at a time, learn deeply, and enjoy the journey. Every great debugger started with a single `ptrace()` call.

**Ferros** — built from the same element as Rust. 🦀
