# Ferros Project Architecture

## Overview
Ferros is structured as a **workspace of multiple crates**, located under the `/crates` directory. Each crate is responsible for a distinct part of the debugger system, enabling modular development, testing, and independent versioning. Each crate should have its own `README.md` describing its purpose and public API.

```
ferros/
├── Cargo.toml               # Workspace definition
├── crates/                  # All crates live here
│   ├── ferros/                     # Command-line interface for Ferros
│   ├── ferros-core/                # Core runtime and process control logic
│   ├── ferros-mir/                 # MIR integration, introspection, and analysis
│   ├── ferros-ui/                  # Optional TUI/GUI for visualization
│   ├── ferros-protocol/            # Communication layer between debugger and frontend
│   └── ferros-utils/               # Shared utilities, logging, config, and helpers
└── tests/                   # Integration tests spanning multiple crates
```

---

## Crate Responsibilities

### **crates/ferros-core**
- Implements low-level debugging primitives.
- Interfaces with OS-level APIs (`ptrace`, `procfs`, or Windows Debug API`).
- Manages breakpoints, stack unwinding, register inspection, and memory mapping.
- Provides async-safe abstractions using `tokio` or `async-std`.

### **crates/ferros-mir**
- Integrates with Rust compiler internals and **MIR (Mid-level Intermediate Representation)**.
- Performs symbolic execution, type inspection, and variable lifetime analysis.
- Exposes APIs for interpreting and visualizing MIR blocks.
- May use `rustc_private` or crates like `rustc_driver` for compiler hooks.

### **crates/ferros-cli**
- Implements the `ferros` command-line tool.
- Supports commands like `run`, `attach`, `step`, `inspect`, `break`, and `watch`.
- Interfaces with the `core` crate for process control and `mir` for Rust-aware inspection.
- Uses `clap` or `argh` for argument parsing.

### **crates/ferros-ui**
- Provides an optional terminal or graphical user interface.
- Displays source view, stack frames, local variables, and heap visualization.
- Built with `ratatui` (for TUI) or `egui` (for GUI).
- Communicates with the `protocol` crate asynchronously.

### **crates/ferros-protocol**
- Defines structured messages exchanged between `core` and frontends (`cli` / `ui`).
- Uses JSON or binary formats (like MessagePack) for efficient communication.
- Enables remote debugging and custom frontend development.

### **crates/ferros-utils**
- Provides shared utilities for configuration, logging, and environment detection.
- Includes common macros, traits, and helper utilities for cross-crate consistency.

---

## Recommended Libraries and Patterns
- **Async Runtime:** `tokio`
- **Error Handling:** `thiserror`, `anyhow`
- **Logging:** `tracing`, `tracing-subscriber`
- **Serialization:** `serde`, `serde_json`
- **CLI:** `clap`
- **Testing:** `proptest`, `insta` (for snapshots)
- **Build Automation:** `cargo-make`, `xtask` pattern for custom scripts

---

## Testing and Benchmarks
- Each crate contains its own `tests/` directory for unit tests.
- The top-level `tests/` directory includes integration tests across crates.
- Use `criterion` for performance benchmarking.
- Continuous Integration should run `cargo test --workspace --all-features`.

---

## Production-Grade Practices
- Enforce formatting and linting: `cargo fmt --all`, `cargo clippy --all`.
- Maintain test coverage across all supported platforms.
- Automate documentation via `cargo doc --workspace --no-deps`.
- Use semantic versioning (`MAJOR.MINOR.PATCH`) for releases.
- Maintain a detailed `CHANGELOG.md` for every release.
- Integrate CI/CD with GitHub Actions or similar tools.

---

## Summary
Ferros follows a **hybrid architecture** combining system-level debugging with MIR-level Rust introspection. The `/crates` directory isolates functionality, ensuring a clean, scalable, and production-ready structure that’s easy for contributors to understand and extend.