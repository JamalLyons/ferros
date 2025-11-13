# ferros

A Rust-native debugger with hybrid MIR and system-level introspection.

## Overview

`ferros` is the command-line interface for the Ferros debugger, providing a unified entry point for debugging Rust applications with both MIR-level and system-level capabilities.

## Installation

```bash
cargo install ferros
```

## Usage

```bash
# Attach to a running process
ferros attach <PID>

# Launch and debug a program
ferros run <program>

# List breakpoints
ferros breakpoints

# Continue execution
ferros continue
```

## Features

- Hybrid MIR and system-level debugging
- Cross-platform support (Linux, macOS, Windows)
- Interactive debugging session
- Breakpoint management
- Stack inspection
- Variable inspection

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.

