# ferros-core

Low-level debugging primitives and process control for Ferros.

## Platform Support

### macOS

**Minimum Requirements:**
- macOS 10.5 (Leopard) or later (for `POSIX_SPAWN_START_SUSPENDED`)
- Recommended: macOS 10.15 (Catalina) or later

**Architecture Support:**
- ✅ ARM64 (Apple Silicon) - Primary target
- ✅ x86_64 (Intel Mac) - Supported for compatibility

**Permissions:**

To debug processes on macOS, you have two options:

1. **Launch processes** (recommended): Use `launch()` to spawn processes under debugger control.
   This doesn't require special permissions.

2. **Attach to running processes**: Requires either:
   - Running with `sudo`
   - Code signing with the `com.apple.security.cs.debugger` entitlement
   
   See `ferros.entitlements` for the entitlement file template.

### Linux

⏳ Coming soon

### Windows

⏳ Coming soon

## Usage

```rust
use ferros_core::platform::macos::MacOSDebugger;
use ferros_core::Debugger;

// Launch a process under debugger control (recommended)
let mut debugger = MacOSDebugger::new()?;
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
