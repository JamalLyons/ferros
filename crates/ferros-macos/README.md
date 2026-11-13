# ferros-macos

The macOS backend of the Ferros debugger. `MacOSBackend` implements the
`ferros_core::Backend` trait on top of Mach APIs (`task_for_pid`, `thread_get_state`,
exception ports, `posix_spawn`). It provides mechanism only; the portable engine
`ferros_core::Session` drives it and owns breakpoints, thread selection, stack traces
and symbolication:

```rust,ignore
let mut session = ferros_core::Session::new(Box::new(ferros_macos::MacOSBackend::new()?));
```

The target is traced with `ptrace(PT_ATTACHEXC)`: signals are reported as stops and can
be passed on or suppressed on resume, and exit is detected with a kqueue watcher that
reaps the process and reports `Exited` or `Killed` through `wait`.

Known gaps, fixed by later tasks: single-step is unsupported (T07, issue #33); `images`
reports only the main executable (issue #34).

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
