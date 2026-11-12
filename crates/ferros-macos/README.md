# ferros-macos

The macOS implementation of the Ferros debugger. `MacOSDebugger` implements the
`ferros_core::Debugger` trait on top of Mach APIs (`task_for_pid`, `thread_get_state`,
exception ports, `posix_spawn`).

All Mach FFI and all `unsafe` code in the workspace lives in this crate; `ferros-core`
holds no OS-specific code. The crate is macOS-only: on any other OS it compiles as an
empty crate.

Only arm64 (Apple Silicon) is tested. The x86_64 register code is carried along but
is untested.

Attaching with `task_for_pid` requires root or the `com.apple.security.cs.debugger`
entitlement in `ferros.entitlements`:

```sh
codesign --entitlements crates/ferros-macos/ferros.entitlements --force --sign - target/debug/ferros
```
