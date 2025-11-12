# Rust Debugger Research

This document covers the key areas of study and research necessary to design and build a **Rust-native debugger** from the ground up. It focuses on understanding the internals of Rust’s compiler pipeline, MIR/MIRI, debugging architectures, and low-level system concepts required for implementing the debugger.

---

## 1. Understanding the Rust Compiler Architecture

### 🔹 Compiler Stages
- **Lexing/Parsing (libsyntax):** How Rust code is tokenized and transformed into an AST.
- **HIR (High-level Intermediate Representation):** What information is preserved and lowered from AST.
- **MIR (Mid-level IR):** The core intermediate representation for Rust used by the borrow checker and optimizations.
- **LLVM IR:** The low-level representation that MIR eventually compiles into.
- **Codegen Backend:** How the compiler emits machine code.

### 🔹 Key Topics to Study
- `rustc` internals and crates (`rustc_middle`, `rustc_codegen_ssa`, `rustc_mir`)
- How Rust stores **debug symbols (DWARF)** and how it interfaces with GDB/LLDB.
- Reading and interpreting **compiler query system** (`ty::context` and incremental compilation`).
- Exploring the `rustc-dev-guide`: <https://rustc-dev-guide.rust-lang.org>

### 🔹 Tools & APIs
- `rustc_private` crates and how to use them.
- `rust-analyzer` architecture for semantic understanding.
- Compiler plugin interface (unstable but educational).

---

## 2. Understanding MIR & MIRI

### 🔹 MIR (Mid-level IR)
- Learn MIR’s structure, representation of variables, control flow, and ownership.
- How borrow checking, type inference, and pattern matching are represented.
- How to interpret MIR to reproduce stack frames, variable lifetimes, and memory state.

### 🔹 Miri
- Study how Miri interprets MIR to execute code at runtime.
- Understand how Miri handles undefined behavior, memory checks, and symbolic execution.
- Investigate Miri’s limitations: performance, native code execution, and external FFI calls.
- Learn from Miri’s architecture for building your own MIR interpreter backend.

---

## 3. Debugger Fundamentals

### 🔹 Low-level Debugging Concepts
- Process memory layout (stack, heap, data, text segments).
- Hardware breakpoints vs software breakpoints.
- CPU registers and stack frames.
- How stepping, pausing, and continuing execution work.
- Signal handling (e.g., SIGTRAP, SIGSEGV).

### 🔹 Debug Information Formats
- **DWARF standard:** structure, location lists, line number tables.
- **PDB (Windows):** how debug symbols are stored differently.
- Parsing DWARF using libraries like `gimli` and `object` in Rust.

### 🔹 System Interfaces
- **Linux/macOS:** `ptrace`, signals, `/proc` filesystem.
- **Windows:** Windows Debugging API (Win32 API).
- Cross-platform process control abstraction.

---

## 4. Learning from Existing Debuggers

### 🔹 Study GDB and LLDB
- High-level debugger architecture (frontend/backend separation).
- Command protocols (MI for GDB, LLDB remote protocol).
- How they manage symbol tables, breakpoints, and stepping.

### 🔹 Study Existing Rust Tools
- How Rust code interacts with GDB/LLDB via DWARF.
- Known limitations (incorrect variable names, optimization mismatch, async stack unwinding).
- Analyze `rust-gdb` and `rust-lldb` wrappers.

### 🔹 Explore Modern Debugging Models
- Debug adapter protocol (DAP) used by VS Code.
- LLDB Remote GDB server design.
- Potential hybrid debugger model (native MIR + binary-level backend).

---

## 5. Core Knowledge to Develop

### 🔹 Rust Internals
- Ownership, lifetimes, and stack layout mapping.
- Trait object layout and vtables.
- Generics monomorphization and its effect on debugging.

### 🔹 OS and Hardware
- Syscalls for process tracing and control.
- Thread management and async stack unwinding.
- How CPU exceptions map to program crashes.

### 🔹 Tooling Ecosystem
- `gimli`, `addr2line`, `object`, and `capstone` crates.
- Symbolication and demangling (`rustc-demangle`).
- Debug adapters (DAP implementation in Rust).

---

## 6. Research References
- Rust Compiler Dev Guide: <https://rustc-dev-guide.rust-lang.org>
- Cargo and Crates.io: Rust <https://doc.rust-lang.org/book/ch14-00-more-about-cargo.html>
- Miri Repository: <https://github.com/rust-lang/miri>
- `gimli` crate: <https://github.com/gimli-rs/gimli>
- LLDB internals: <https://lldb.llvm.org/design.html>
- DWARF Spec: <https://dwarfstd.org/doc/DWARF5.pdf>
- GDB Remote Protocol: <https://sourceware.org/gdb/onlinedocs/gdb/Remote-Protocol.html>
- ptrace(2) man page and Linux procfs documentation.

---
