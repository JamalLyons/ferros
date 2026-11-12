# ferros-core

Low-level debugging primitives and process control for Ferros.

## Platform Support

`ferros-core` holds no OS-specific code and no `unsafe`. Each operating system is
implemented in its own crate.

### macOS

Implemented by [`ferros-macos`](../ferros-macos/), which also holds the debugger
entitlement file (`ferros.entitlements`) and the macOS requirements.

### Linux

⏳ Coming soon

### Windows

⏳ Coming soon

## Usage

```rust
use ferros_core::Debugger;

// The debugger implementation comes from a platform crate.
let debugger: &mut dyn Debugger = /* platform debugger */;

// Launch a process under debugger control (recommended)
debugger.launch("/usr/bin/echo", &["echo", "Hello, world!"])?;
// Process is now suspended and ready for debugging

// Or attach to an existing process (requires permissions)
debugger.attach(ProcessId::from(12345))?;

// Inspect the process
let regs = debugger.read_registers()?;
println!("Program counter: 0x{:x}", regs.pc);
```

## Features

- ✅ Process attachment and launching
- ✅ Register reading (ARM64 and x86_64)
- ✅ Memory reading and writing
- ✅ Memory region enumeration
- ✅ Thread enumeration
- ⏳ Register writing (coming soon)
- ⏳ Breakpoints (coming soon)
- ⏳ Single-step execution (coming soon)
