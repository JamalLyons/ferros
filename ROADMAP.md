# 🦀 Ferros — Rust Native Debugger Development Roadmap

This roadmap outlines the milestones for developing **Ferros**, a Rust-native debugger built on a hybrid architecture that unifies low-level process control with MIR-level interpretation.

Use this roadmap to track progress, create GitHub issues, and guide contributors.

---

## 📘 Phase 1 — Research & Foundations
> Goal: Build deep understanding of Rust’s compiler internals, debugging standards, and architectural requirements.

- [ ] Research the Rust compiler pipeline (HIR → MIR → LLVM IR).
- [ ] Study `rustc_codegen_ssa` and `rustc_mir` crates.
- [ ] Learn DWARF, ELF, and PDB debugging formats.
- [ ] Review how `gdb` and `lldb` integrate with compilers.
- [ ] Explore `ptrace`, Mach ports, and Windows debugging APIs.
- [ ] Investigate existing projects like `rr`, `probe-rs`, and `miri`.
- [ ] Define target platforms for initial release (Linux first, others later).
- [ ] Document research findings in `/docs/architecture/overview.md`.

---

## ⚙️ Phase 2 — Core Infrastructure Setup
> Goal: Prepare the foundational project structure and CI pipeline.

- [ ] Create `ferros-core` crate (engine and architecture scaffolding).
- [ ] Create `ferros-cli` crate (command-line interface).
- [ ] Set up Cargo workspace for modular development.
- [ ] Initialize GitHub repo with README, LICENSES (MIT & Apache), and CONTRIBUTING.md.
- [ ] Set up CI with GitHub Actions (build, format, clippy).
- [ ] Add `rustfmt` and `clippy` configs for style enforcement.
- [ ] Add basic unit testing and integration testing structure.

---

## 🧠 Phase 3 — Debugger Core (Process Control Layer)
> Goal: Implement low-level process control, breakpoints, and memory inspection.

### Linux (ptrace) Backend
- [ ] Implement process attach/detach using `ptrace`.
- [ ] Implement reading/writing registers.
- [ ] Implement memory read/write operations.
- [ ] Add breakpoint support via software interrupts.
- [ ] Add stepping and continuing execution.
- [ ] Handle signal events (SIGTRAP, SIGSEGV, etc.).
- [ ] Parse `/proc/[pid]/maps` for memory regions.

### Cross-Platform Abstraction
- [ ] Design `DebuggerBackend` trait for backend implementations.
- [ ] Abstract OS-specific APIs (Linux, macOS, Windows).
- [ ] Add dummy stubs for macOS (Mach) and Windows (WinDbg) for future use.

---

## 🧩 Phase 4 — Symbol and Type System
> Goal: Parse and map debug symbols to Rust-level constructs.

- [ ] Implement DWARF parser (or integrate with `gimli` crate).
- [ ] Extract function names, line info, and variable symbols.
- [ ] Parse type info for structs, enums, and primitives.
- [ ] Create `SymbolTable` abstraction layer.
- [ ] Map DWARF symbols to Rust source locations.
- [ ] Add caching for symbol lookup.

---

## 🧬 Phase 5 — MIR-Level Debugging Engine
> Goal: Integrate with the Rust compiler to interpret and simulate MIR execution.

- [ ] Build `ferros-mir` crate for MIR analysis.
- [ ] Use `rustc_private` to access MIR APIs (initially via nightly toolchain).
- [ ] Implement MIR IR loader for a compiled crate.
- [ ] Create an interpreter for control flow (basic blocks, terminators).
- [ ] Visualize ownership and lifetime states.
- [ ] Enable mixed-mode debugging: native + MIR-level inspection.
- [ ] Add symbolic variable tracking for unsafe or async code.

---

## 🧰 Phase 6 — CLI Interface
> Goal: Create an intuitive command-line interface for debugging Rust binaries.

- [ ] Implement `ferros run <target>` command (launch a program).
- [ ] Implement `ferros attach <pid>` for attaching to live processes.
- [ ] Add `break`, `continue`, `next`, and `step` commands.
- [ ] Add `inspect <var>` and `print <expr>` for variable inspection.
- [ ] Integrate colored terminal output and structured error messages.
- [ ] Add tab completion (via `rustyline` or similar library).
- [ ] Write CLI help menu and subcommand docs.

---

## 💻 Phase 7 — IDE Integration (DAP)
> Goal: Build Debug Adapter Protocol support for VS Code and other IDEs.

- [ ] Create `ferros-dap` crate for Debug Adapter Protocol integration.
- [ ] Implement message protocol between Ferros and client (VS Code).
- [ ] Map DAP commands to Ferros core actions (breakpoints, step, etc.).
- [ ] Add variable and stack frame reporting.
- [ ] Test with `vscode-debugadapter` and `nvim-dap`.
- [ ] Publish extension manifest for early testing.

---

## 🧪 Phase 8 — Testing & Validation
> Goal: Ensure stability and correctness across supported features.

- [ ] Create test binaries in `/examples` for debugging tests.
- [ ] Write automated tests for ptrace backend.
- [ ] Add integration tests for CLI commands.
- [ ] Test symbol resolution accuracy with DWARF samples.
- [ ] Implement snapshot tests for MIR interpreter.
- [ ] Run Ferros under itself to test meta-debugging scenarios.

---

## 📘 Phase 9 — Documentation & Developer Experience
> Goal: Build a welcoming and well-documented project.

- [ ] Write detailed `CONTRIBUTING.md` for new contributors.
- [ ] Add `ARCHITECTURE.md` with diagrams.
- [ ] Document public APIs using Rustdoc.
- [ ] Create `/docs/user-guide.md` for end users.
- [ ] Create `/docs/dev-guide.md` for contributors.
- [ ] Set up GitHub Pages or Docusaurus site for documentation.
- [ ] Add badges (build status, license, crates.io, etc.) to README.

---

## 🚀 Phase 10 — Release and Community
> Goal: Launch the first public release and build contributor momentum.

- [ ] Tag and publish `v0.1.0` on GitHub and crates.io.
- [ ] Announce on Reddit (r/rust), Twitter/X, and the Rust Discord.
- [ ] Write introductory blog post: *“Introducing Ferros: The Rust-Native Debugger.”*
- [ ] Gather feedback and open RFCs for new features.
- [ ] Begin planning v0.2 features (performance improvements, UI integration).

---

### 🧭 Notes
- Use GitHub issues for each task or feature.
- Use labels like `good first issue`, `help wanted`, and `discussion`.
- Link roadmap tasks to PRs as they are completed.
- Keep `/ROADMAP.md` updated each release cycle.

---

> **Ferros** — built from the same element as Rust.
