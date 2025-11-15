# ferros-core

Low-level debugging primitives and process control for Ferros.

## Overview

`ferros-core` provides the foundational debugging capabilities for the Ferros debugger, including:

- Process control and manipulation
- Breakpoint management
- Stack unwinding
- Register inspection
- Memory mapping and inspection
- Platform-specific debugging APIs (ptrace, Mach ports, Windows Debug API)

## Usage

Add `ferros-core` to your `Cargo.toml`:

```toml
[dependencies]
ferros-core = "0.0.0"
```

## Platform Support

- **macOS**: 10.9+ (Mavericks) for Intel, 11.0+ (Big Sur) for Apple Silicon
- **Linux**: Planned (ptrace)
- **Windows**: Planned (WinDbg APIs)

## Requirements

- **Rust**: Edition 2021 (Rust 1.56.0 or newer)
- **macOS**: Uses Mach APIs (`task_for_pid`, `mach_vm_region`, etc.)
  - Requires debugging entitlements or `sudo` for process attachment
  - See [macOS Debugging Entitlements](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.

