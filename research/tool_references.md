# Rust Debugger Tools & Crate Reference

This document lists and explains key tools, crates, and libraries useful for building a **Rust-native debugger**. It’s divided by purpose — from low-level system interfaces to visualization and IDE integration.

---

## 🧩 Core Debugging & Process Control

### 🔹 `nix`
- Provides POSIX API bindings for Linux/macOS.
- Useful for handling `ptrace`, signals, process management, and file descriptors.
- Docs: <https://docs.rs/nix>

### 🔹 `windows` (crate)
- Official Rust bindings for Windows API.
- Used for implementing Windows Debugging API equivalents (CreateProcess, WaitForDebugEvent, etc.).
- Docs: <https://github.com/microsoft/windows-rs>

### 🔹 `libc`
- Lightweight bindings for C system calls and constants.
- Often used alongside `nix` for fine-grained control.

### 🔹 `ptrace` crate (optional)
- Provides direct wrappers for `ptrace` system calls.
- Ideal for low-level debugging on Linux.

---

## 🧠 Symbol Parsing & Debug Info

### 🔹 `gimli`
- The core library for parsing DWARF debug information.
- Supports reading symbols, type info, and line mappings.
- Used by projects like `addr2line`, `backtrace`, and `miri`.
- Docs: <https://github.com/gimli-rs/gimli>

### 🔹 `object`
- Cross-platform object file parser (ELF, Mach-O, PE).
- Allows you to extract sections, symbols, and relocation info.
- Docs: <https://github.com/gimli-rs/object>

### 🔹 `addr2line`
- Uses DWARF + `gimli` to resolve instruction addresses to source code.
- Great for implementing stack trace and breakpoint line mapping.
- Docs: <https://docs.rs/addr2line>

### 🔹 `rustc-demangle`
- Demangles Rust’s mangled symbol names for readable output.
- Docs: <https://docs.rs/rustc-demangle>

---

## ⚙️ MIR / Compiler Interaction

### 🔹 `rustc_interface` and `rustc_driver`
- Allow access to Rust compiler internals and custom compilation pipelines.
- Use for reading MIR, HIR, and symbol tables directly.
- Docs: <https://doc.rust-lang.org/nightly/nightly-rustc/rustc_interface>

### 🔹 `miri`
- Official Rust MIR interpreter. Study its codebase to learn how to execute MIR safely.
- Repo: <https://github.com/rust-lang/miri>

### 🔹 `rust-analyzer`
- Language server for Rust; provides semantic analysis and code intelligence.
- Useful reference for mapping source code to compiler structures.
- Repo: <https://github.com/rust-lang/rust-analyzer>

---

## 🧩 Frontend & UI Components

### 🔹 `ratatui` or `crossterm`
- For building terminal-based UIs with panels, colors, and interactive input.
- Ideal for creating a TUI debugger frontend.

### 🔹 `eframe` + `egui`
- GUI framework in Rust that’s easy to integrate with custom logic.
- Can serve as a base for a desktop debugger app.

### 🔹 `serde` + `serde_json`
- For serializing debugger state, breakpoints, and communication with IDEs.

---

## 💻 IDE Integration & Protocols

### 🔹 `tower-lsp`
- Rust implementation of the Language Server Protocol (LSP).
- Provides a good starting point for implementing DAP (Debug Adapter Protocol) support.

### 🔹 Debug Adapter Protocol (DAP)
- Protocol used by VSCode and JetBrains for debugging communication.
- Spec: <https://microsoft.github.io/debug-adapter-protocol>

### 🔹 `tokio` or `async-std`
- For async communication, socket handling, and remote debugging.
- Use for DAP server or remote target support.

---

## 🧪 Testing, Profiling, and Utilities

### 🔹 `criterion`
- Benchmarking framework for Rust — useful for profiling debugger performance.

### 🔹 `proptest` / `quickcheck`
- Property-based testing for debugger commands and symbol parsing.

### 🔹 `tracing` + `tracing-subscriber`
- Structured logging system for Rust.
- Perfect for verbose mode or debugging debugger internals.

---

## 🧱 Build & Tooling Ecosystem

### 🔹 `cargo` subcommand integration
- Build your debugger as a `cargo debug` subcommand.
- Explore `cargo-plugin` examples like `cargo-edit` or `cargo-audit`.

### 🔹 Continuous Integration Tools
- Use `cross` for cross-compiling the debugger.
- GitHub Actions or `cargo-nextest` for testing across OS targets.

---

## 🧩 Optional / Advanced Crates

### 🔹 `capstone`
- Disassembly engine for multiple architectures.
- Useful for low-level inspection and instruction tracing.

### 🔹 `iced-x86`
- High-performance x86/x64 disassembler in Rust.
- Use for showing current instruction during stepping.

### 🔹 `memmap2`
- For efficient binary file mapping and analysis.

---

## 📚 Suggested Learning Projects
- Write a small program that uses `gimli` to list all symbols in a binary.
- Build a CLI tool that uses `ptrace` to pause a process and read its registers.
- Parse a MIR dump using `rustc_interface` and print its control flow graph.

---
