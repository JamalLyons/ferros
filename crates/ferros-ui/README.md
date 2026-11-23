# ferros-ui

Terminal User Interface (TUI) for the Ferros debugger.

## Status

`ferros-ui` is currently **parked and excluded from the Cargo workspace** (`exclude = ["crates/ferros-ui"]` in the root `Cargo.toml`).

The initial implementation was written using `ratatui` against the retired `Debugger` trait, which is no longer in `ferros-core`. The crate is excluded from the workspace and does not build against `Session`. `ferros launch` and `ferros attach` open the line-oriented REPL in `crates/ferros`, not this TUI.

## Planned Rebuild

The interactive UI is being redesigned to interface with `Session`:

- **Views**:
  - Overview / status panel
  - CPU registers table
  - Thread list and navigation
  - Memory regions inspection
  - Source code and breakpoint gutter
  - Process stdout/stderr capture panel
- **Architecture**:
  - Tokio async runtime driving event loop and background stream capture
  - Synchronous `ratatui` terminal rendering using cached snapshot state to prevent blocking the async runtime
  - Clean terminal restoration on shutdown

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.
