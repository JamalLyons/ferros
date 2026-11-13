# ferros

The command-line interface for the Ferros debugger.

## Overview

`ferros` is the user-facing CLI binary for the Ferros debugging platform. It parses commands, initializes structured logging via `ferros-utils`, instantiates the platform backend (on macOS, `ferros-macos`), and controls target execution through `ferros-core::Session`.

## Building

Ferros is not published to crates.io yet. Build from source:

```bash
cargo build -p ferros
```

On macOS, running or debugging external processes requires the binary to be ad-hoc signed with the debugger entitlement (`crates/ferros-macos/ferros.entitlements`):

```bash
just sign-debugger target/debug/ferros
# or: codesign --entitlements crates/ferros-macos/ferros.entitlements --force --sign - target/debug/ferros
```

## Usage

```bash
# Launch a program under the debugger
cargo run -p ferros -- launch <program> [args...]

# Attach to an existing process by PID
cargo run -p ferros -- attach <pid>

# Show debugger information
cargo run -p ferros -- info

# View or locate the log directory
cargo run -p ferros -- find-logs
```

> **Note**: The interactive terminal UI (`ferros-ui`) is currently parked and excluded from the workspace while being redesigned around `Session`. Both `launch` and `attach` currently operate in headless mode: inspecting the target at entry, printing debugger, thread, and memory region information, and detaching.

## Subcommands

- **`launch <program> [args...] [--headless]`**: Launch a new process under debugger control.
- **`attach <pid> [--headless]`**: Attach to a running process by PID.
- **`registers`**: Display CPU registers (requires persistent session).
- **`memory <address> [--length <bytes>]`**: Read memory from process.
- **`regions`**: List memory regions (segments) of the process.
- **`threads`**: List all threads in the target process.
- **`suspend`**: Suspend process execution.
- **`resume`**: Resume process execution.
- **`detach`**: Detach from target process.
- **`info`**: Show target status, architecture, and debugger details.
- **`find-logs`**: Print the path to the Ferros log directory.

## Platform Support

- **macOS (Apple Silicon arm64)**: Currently supported backend (`crates/ferros-macos`).
- **Linux & Windows**: Planned.

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.
