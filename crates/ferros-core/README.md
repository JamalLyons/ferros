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

- Linux (ptrace)
- macOS (Mach ports)
- Windows (WinDbg APIs)

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.

