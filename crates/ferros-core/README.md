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

`Session` is the portable engine. It drives a `Box<dyn Backend>` that comes from a
platform crate (on macOS, `ferros_macos::MacOSBackend`).

```rust,ignore
use ferros_core::{LaunchSpec, Session};

// The backend comes from a platform crate.
let mut session = Session::new(Box::new(ferros_macos::MacOSBackend::new()?));

// Launch a process, stopped before its first instruction. The arguments exclude argv[0].
session.launch(&LaunchSpec::new("/bin/echo").arg("Hello, world!"))?;

// Or attach to an existing process (requires permissions):
// session.attach(ProcessId::from(12345))?;

// Inspect the process
let regs = session.read_registers()?;
println!("Program counter: {}", regs.pc);

// Release it; it keeps running.
session.detach()?;
```

## Features

- ✅ Process launching and attaching
- ✅ Register reading and writing (ARM64; x86_64 untested)
- ✅ Memory reading and writing
- ✅ Memory region enumeration
- ✅ Thread enumeration
- ✅ Software breakpoints (insert, remove, hit counts)
- ⏳ Stepping over breakpoints and single-step execution (T07, issue #33)
- ⏳ Process exit detection and signal pass-through (T06, issue #32)
