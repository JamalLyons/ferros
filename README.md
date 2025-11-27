<div align="center">

# 🦀 Ferros

**A Rust-native debugger built from the same element as Rust itself.**

[![GitHub stars](https://img.shields.io/github/stars/jamallyons/ferros.svg?style=social&label=Star)](https://github.com/jamallyons/ferros)
![Crates.io Downloads (recent)](https://img.shields.io/crates/dr/ferros)
[![Lines of Code](https://img.shields.io/endpoint?url=https%3A%2F%2Ftokei.kojix2.net%2Fbadge%2Fgithub%2Fjamallyons%2Fferros%2Flines)](https://tokei.kojix2.net/github/jamallyons/ferros)
![X (formerly Twitter) Follow](https://img.shields.io/twitter/follow/ferros_rs)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)

<img src="https://raw.githubusercontent.com/JamalLyons/ferros/refs/heads/master/assets/ferros-logo.png" alt="Ferros Logo" width="300" style="border-radius: 50%;">

**Ferros** (from *ferrum* — Latin for iron) is a next-generation debugger designed specifically for the Rust programming language.

Unlike traditional debuggers that retrofit C-style semantics onto Rust programs, Ferros is built from the ground up to understand Rust's unique memory model, lifetimes, ownership system, and mid-level intermediate representation (MIR).

</div>

---

## Mission

To give Rust developers a **first-class, language-native debugging experience** — fast, intuitive, and designed for Rust's safety guarantees.

Ferros aims to become the Rust ecosystem's standard debugging platform, complementing `cargo` and integrating cleanly with editors, IDEs, and build tools.

---

## What Makes Ferros Different?

While existing Rust debugging solutions are either platform-limited or retrofitted from C/C++ debuggers, Ferros is built **from the ground up** for Rust developers.

| Solution | Limitations |
|----------|-------------|
| **BugStalker** | Linux-only, no macOS or Windows support |
| **GDB/LLDB** | C/C++ semantics, doesn't understand Rust's ownership model |
| **CodeLLDB** | Generic debugger extension, no Rust-specific features |

**Ferros brings:**
- ✅ **Cross-Platform First**: macOS support now, with Linux and Windows planned
- ✅ **Rust-Native Understanding**: Built to understand ownership, lifetimes, borrows, and MIR
- ✅ **MIR-Level Debugging**: Debug at the compiler's intermediate representation
- ✅ **Hybrid Architecture**: Combines symbolic debugging and interpreted execution
- ✅ **Modern Design**: Designed for Rust's safety guarantees from day one

---

## Current Status

Ferros is currently in **active development** with core infrastructure in place.

**See [ROADMAP.md](./ROADMAP.md) for detailed development milestones and progress tracking.**

---

## Architecture

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

## Quick Start

> **Note:** Ferros is currently in early development. Installation and usage instructions will be available in future releases.

Once available, you'll be able to:

```bash
# Install via cargo
cargo install ferros

# Debug your Rust project
cargo ferros [ferros args...] launch <program> [program args...]
```

### Requirements

- **Rust**: Edition 2021 (Rust 1.56.0 or newer)
- **macOS**: 10.9+ (Mavericks) for Intel Macs, 11.0+ (Big Sur) for Apple Silicon
- **Linux**: Planned (ptrace support)
- **Windows**: Planned (WinDbg API support)

> **Note**: On macOS, debugging other processes requires either:
> - Running with `sudo`, or
> - Granting debugging entitlements (`com.apple.security.cs.debugger`)

---

## Documentation

- **[Architecture](./docs/architecture.md)** — Deep dive into Ferros' design and structure
- **[Roadmap](./ROADMAP.md)** — Development milestones and planned features
- **[Contributing](./CONTRIBUTING.md)** — Guidelines for contributing to Ferros

---

## Contributing

We welcome contributions! Ferros is built with ❤️ by the Rust community.

Check out [CONTRIBUTING.md](./CONTRIBUTING.md).

---

## License

Licensed under the [Apache License](./LICENSE).

---

## Acknowledgments

Ferros draws inspiration from:
- The Rust compiler's MIR representation
- Existing debuggers like `gdb`, `lldb`, and `rr`
- Projects like `probe-rs` and `miri`

---

<div align="center">

**Ferros** — built from the same element as Rust.

[⭐ Star us on GitHub](https://github.com/jamallyons/ferros) • [📖 Read the Docs](./docs) • [🐛 Report Issues](https://github.com/jamallyons/ferros/issues)

</div>
