<div align="center">

# 🦀 Ferros

**A Rust-native debugger built from the same element as Rust itself.**

[![GitHub stars](https://img.shields.io/github/stars/jamallyons/ferros.svg?style=social&label=Star)](https://github.com/jamallyons/ferros)
[![GitHub forks](https://img.shields.io/github/forks/jamallyons/ferros.svg?style=social&label=Fork)](https://github.com/jamallyons/ferros/fork)
[![GitHub issues](https://img.shields.io/github/issues/jamallyons/ferros.svg)](https://github.com/jamallyons/ferros/issues)
[![GitHub pull requests](https://img.shields.io/github/issues-pr/jamallyons/ferros.svg)](https://github.com/jamallyons/ferros/pulls)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)

<img src="https://raw.githubusercontent.com/JamalLyons/ferros/refs/heads/master/assets/ferros-logo.png" alt="Ferros Logo" width="300" style="border-radius: 50%;">

**Ferros** (from *ferrum* — Latin for iron) is a next-generation debugger designed specifically for the Rust programming language.

Unlike traditional debuggers that retrofit C-style semantics onto Rust programs, Ferros is built from the ground up to understand Rust's unique memory model, lifetimes, ownership system, and mid-level intermediate representation (MIR).

</div>

---

## 🎯 What Makes Ferros Different?

While existing Rust debugging solutions are either platform-limited or retrofitted from C/C++ debuggers, Ferros is built **from the ground up** for Rust developers.

### Why Not Use Existing Debuggers?

| Solution | Limitations |
|----------|-------------|
| **BugStalker** | Linux-only, no macOS or Windows support |
| **GDB/LLDB** | C/C++ semantics, doesn't understand Rust's ownership model |
| **CodeLLDB** | Generic debugger extension, no Rust-specific features |

### What Ferros Brings

✅ **Cross-Platform First**: macOS support now, with Linux and Windows planned — unlike Linux-only alternatives  
✅ **Rust-Native Understanding**: Built to understand ownership, lifetimes, borrows, and MIR — not retrofitted C semantics  
✅ **MIR-Level Debugging**: Debug at the compiler's intermediate representation for precise control flow analysis  
✅ **Hybrid Architecture**: Combines symbolic debugging and interpreted execution in a single tool  
✅ **Modern Design**: Designed for Rust's safety guarantees from day one, not adapted from older debuggers  

> **Built from the same element as Rust.**  
> A debugger that speaks Rust's language.

---

## ✨ Mission

To give Rust developers a **first-class, language-native debugging experience** — fast, intuitive, and designed for Rust's safety guarantees.

Ferros aims to become the Rust ecosystem's standard debugging platform, complementing `cargo` and integrating cleanly with editors, IDEs, and build tools.

---

## 🔧 Key Features

| Feature | Description |
|---------|-------------|
| **Rust-Native Inspection** | Understands ownership, borrows, lifetimes, and async tasks directly |
| **MIR Integration** | Debug at the mid-level IR to analyze compiler-accurate control flow |
| **Hybrid Execution Engine** | Combine symbolic simulation with native process control for precise debugging |
| **Zero-Cost Breakpoints** | Lightweight traps integrated with Rust's runtime model |
| **Memory Safety Visualization** | Visualize move semantics, borrows, and drops in real time |
| **Cross-Platform Support** | Linux (ptrace), macOS (Mach ports), and Windows (WinDbg APIs) |
| **Cargo Integration** | Seamless `cargo ferros` workflow for launching and inspecting projects |

> **Note:** Ferros is currently in active development. See [ROADMAP.md](./ROADMAP.md) for current status.

---

## 🏗️ Architecture

Ferros is structured as a **workspace of multiple crates**, each with a specific responsibility:

```
ferros/
├── crates/
│   ├── ferros/          # Command-line interface
│   ├── ferros-core/     # Low-level debugging primitives & process control
│   ├── ferros-mir/      # MIR integration & analysis
│   ├── ferros-ui/       # Optional TUI/GUI for visualization
│   ├── ferros-protocol/ # Communication layer
│   └── ferros-utils/    # Shared utilities & helpers
```

📖 **Learn more:** See [ARCHITECTURE.md](./docs/architecture.md) for detailed architecture documentation.

---

## 🚀 Quick Start

> **Coming Soon!** Ferros is currently in early development. Check back soon for installation instructions.

Once available, you'll be able to:

```bash
# Install via cargo
cargo install ferros

# Debug your Rust project
cargo ferros run

# Attach to a running process
ferros attach <pid>
```

---

## 📚 Documentation

- **[Architecture](./docs/architecture.md)** — Deep dive into Ferros' design and structure
- **[Roadmap](./ROADMAP.md)** — Development milestones and planned features
- **[Contributing](./CONTRIBUTING.md)** — Guidelines for contributing to Ferros

---

## 🤝 Contributing

We welcome contributions! Ferros is built with ❤️ by the Rust community.

📖 **Getting Started:** Check out [CONTRIBUTING.md](./CONTRIBUTING.md) for:
- Development setup instructions
- Code style guidelines
- Testing requirements
- How to submit pull requests

**Looking for your first contribution?** Check out issues labeled [`good first issue`](https://github.com/jamallyons/ferros/labels/good%20first%20issue)!

---

## 📄 License

Licensed under the [Apache License, Version 2.0](./LICENSE).

---

## 🙏 Acknowledgments

Ferros draws inspiration from:
- The Rust compiler's MIR representation
- Existing debuggers like `gdb`, `lldb`, and `rr`
- Projects like `probe-rs` and `miri`

---

<div align="center">

**Ferros** — built from the same element as Rust.

[⭐ Star us on GitHub](https://github.com/jamallyons/ferros) • [📖 Read the Docs](./docs) • [🐛 Report Issues](https://github.com/jamallyons/ferros/issues)

</div>
