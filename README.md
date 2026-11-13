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

## Goals

Ferros aims to be a Rust-aware debugger: ownership, lifetimes, and MIR-level understanding, starting on macOS with Linux and Windows planned. These are goals, not current features.

---

## Current Status

Ferros is **early and actively developed**. What is verified today:

- The workspace builds cleanly on nightly Rust (Edition 2024).
- **Process lifecycle & control**: On macOS arm64, launching suspended, attaching by PID, task port acquisition, thread and memory-region enumeration, resuming, detaching, timed wait, process exit detection and reaping, signal pass-through/suppression, interrupt and kill (verified by `tests/lifecycle.rs` and `tests/smoke.rs`).
- **Mach exception loop**: Queueing simultaneous stops, exception port rights lifecycle, port restoration on detach, and signal delivery (verified by `tests/exceptions.rs`).
- **Single-stepping**: Hardware single-step (`MDSCR_EL1.SS`), per-thread single-stepping while other threads remain suspended (verified by `tests/single_step.rs`).
- **Software breakpoints**: Trap insertion, hit counting, transparent trap-hiding in memory reads, automatic serial step-over on resume, and trap restoration on detach and drop (verified by `tests/breakpoints.rs`).
- **Platform boundary**: Zero unsafe code and zero OS dependencies in `ferros-core`, strictly enforced by `just check-boundary` and `tests/platform_boundary.rs`.

The interactive TUI is being rebuilt: `ferros launch` and `attach` currently run in headless mode (inspect and exit/detach). Areas currently in progress include DWARF/symbol ingestion, call stack unwinding, and Linux support.

The plan is tracked in epic [#26](https://github.com/JamalLyons/ferros/issues/26). See [ROADMAP.md](./ROADMAP.md) for milestone tracking.

---

## Architecture

Ferros is structured as a **workspace of multiple crates**, each with a specific responsibility:

```
ferros/
├── crates/
│   ├── ferros/          # Command-line interface
│   ├── ferros-core/     # Portable debugging engine (no OS-specific code)
│   ├── ferros-macos/    # macOS implementation (Mach APIs)
│   ├── ferros-tests/    # Integration test suite, harness, and fixture programs
│   ├── ferros-utils/    # Shared utilities & structured logging
│   ├── ferros-mir/      # MIR integration & analysis (reserved)
│   ├── ferros-protocol/ # Communication layer (reserved)
│   └── ferros-ui/       # TUI (excluded from workspace while being rebuilt)
```

📖 **Learn more:** See [ARCHITECTURE.md](./docs/architecture.md) for detailed architecture documentation.

---

## Quick Start

Not published to crates.io yet. Build from source:

```bash
cargo build --workspace
cargo run -p ferros -- launch --headless <program> [args...]
```

### Requirements

- **Rust**: Edition 2024; the nightly toolchain is selected automatically by [`rust-toolchain.toml`](./rust-toolchain.toml)
- **macOS** (Apple Silicon is what CI and development use): the only supported platform today
- **Linux / Windows**: not implemented

> **Note**: On macOS, debugging other processes needs the binary to be code-signed with the debugger entitlement. Ad-hoc signing with `crates/ferros-macos/ferros.entitlements` is the verified path.

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
