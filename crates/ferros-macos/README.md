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

`resume` honours every thread action: a `Step` thread executes one instruction (arm64
hardware single-step, `MDSCR_EL1.SS`) and stops with `SingleStep`, while `Suspend`
threads stay suspended and retain any unanswered Unix signal replies. Hardware
exception replies are sent while the task remains suspended, allowing external SIGKILL
at breakpoint stops. SIGKILL during a held Unix signal stop remains deferred (issue #56).

Known gap, fixed by a later task: `images` reports only the main executable (issue #34).

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

## Detach failures and signal behavior

Detach resumes the target even if it was job-control stopped before attach. Its private
SIGSTOP is undone with SIGCONT, which a target's SIGCONT handler can observe. Held user
signals are discarded.

If ending tracing or restoring exception ports fails, teardown still attempts every
cleanup step. Explicit detach returns `BackendError::DetachCleanup` after releasing local
resources; both backend and Session then have no process. This error does not guarantee
that the target was fully released or that all its handlers were restored. Failures
before teardown leave the session stopped. Drop logs restore failures because it cannot
return an error.

If replying to an exception and resuming the task both fail, resume returns the task
resume error and stays stopped. The reply error remains queued once for wait, or for the
next Session resume's initial event drain.
