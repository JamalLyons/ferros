# 🦀 Ferros
**A Rust-native debugger built from the same element as Rust itself.**

Ferros (Derived from ferrum - Latin for iron) is a next-generation debugger designed specifically for the Rust programming language.
Unlike traditional debuggers that retrofit C-style semantics onto Rust programs, Ferros is built from the ground up to understand Rust’s unique memory model, lifetimes, ownership system, and mid-level intermediate representation (MIR).

Ferros merges symbolic debugging and interpreted execution in a single hybrid architecture — capable of inspecting compiled binaries *and* running MIR-level simulations for deep program analysis.

---

## ✨ Mission
To give Rust developers a **first-class, language-native debugging experience** — fast, intuitive, and designed for Rust’s safety guarantees.
Ferros aims to become the Rust ecosystem’s standard debugging platform, complementing `cargo` and integrating cleanly with editors, IDEs, and build tools.

---

## 🔧 Key Features (Planned)
- **Rust-Native Inspection:** Understands ownership, borrows, lifetimes, and async tasks directly.
- **MIR Integration:** Debug at the mid-level IR to analyze compiler-accurate control flow.
- **Hybrid Execution Engine:** Combine symbolic simulation with native process control for precise debugging.
- **Zero-Cost Breakpoints:** Lightweight traps integrated with Rust’s runtime model.
- **Memory Safety Visualization:** Visualize move semantics, borrows, and drops in real time.
- **Cross-Platform Support:** Linux (ptrace), macOS (Mach ports), and Windows (WinDbg APIs).
- **Cargo Integration:** Seamless `cargo ferros` workflow for launching and inspecting projects.

---

## 🧱 Architecture Overview
Ferros uses a **Hybrid Debugging Model**, combining two subsystems:
- **Ferros Core:** a low-level process controller and symbol interpreter that interfaces with DWARF, ELF, and PDB data.
- **Ferros MIR Engine:** a lightweight interpreter that replays Rust’s Mid-level Intermediate Representation to simulate logic flow safely and accurately.

This hybrid model allows Ferros to transition smoothly between real and symbolic execution, providing precise control and advanced introspection capabilities.

---

## 🚀 Roadmap

Reference the [ROADMAP.md](./ROADMAP.md) file.

---

## 🤝 Contributing
Ferros is in early development and welcomes contributions from systems programmers, compiler engineers, and Rust enthusiasts.
We’re building this project openly — follow our roadmap, join discussions, and help shape the future of Rust debugging.

---

### License

[Apache 2.0](./LICENSE)

---

### Tagline
> **Ferros** — built from the same element as Rust.
> A debugger that speaks Rust’s language.

---
