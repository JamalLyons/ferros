//! # ferros-macos
//!
//! macOS-specific debugger backend using Mach APIs. [`MacOSBackend`] implements the
//! portable [`ferros_core::Backend`] trait: process control, register and memory access,
//! and stop reporting (mechanism only). The portable engine, [`ferros_core::Session`],
//! drives it and holds the policy (state machine, software breakpoint table, stack
//! traces, symbolication):
//!
//! ```rust,no_run
//! use ferros_core::{LaunchSpec, Session};
//! use ferros_macos::MacOSBackend;
//!
//! let mut session = Session::new(Box::new(MacOSBackend::new()?));
//! session.launch(&LaunchSpec::new("/bin/echo").arg("hello"))?;
//! let regs = session.read_registers()?;
//! println!("stopped at entry, pc = {}", regs.pc);
//! session.detach()?; // the process keeps running
//!
//! # Ok::<(), ferros_core::error::DebuggerError>(())
//! ```
//!
//! ## Backend behaviour
//!
//! - **Launch** uses `posix_spawn` with `POSIX_SPAWN_START_SUSPENDED`; `LaunchSpec::program`
//!   is argv\[0\], and its environment, working directory and stdio specs are honoured.
//! - **Stops**: the exception handler thread suspends the whole task (`task_suspend`)
//!   before it reports a Mach exception, so every thread is stopped when `wait` returns an
//!   event. Threads that raised exceptions at the same moment are reported together: one
//!   event each, queued for `wait` before the target runs again. `EXC_BREAKPOINT` is
//!   reported as `StopEvent::Breakpoint` with the raw pc, or as `StopEvent::SingleStep`
//!   when its codes are those of a completed hardware step; faults become
//!   `StopEvent::Signal`. A failed exception reply is returned by the next `wait` as
//!   `BackendError::Os`, and stops keep being reported.
//! - **Resume** honours every `ThreadAction`. `Step` sets the arm64 hardware single-step
//!   bit (`MDSCR_EL1.SS`) of the thread, which then executes one instruction and stops
//!   with `StopEvent::SingleStep`. `Suspend` keeps the thread `thread_suspend`ed while the
//!   others run. When that is the default and some thread runs, a thread created during
//!   the resume is held at `_thread_start` or `_start_wqthread` in `libsystem_pthread`
//!   until a later resume lets it execute that first instruction. Continuing every
//!   thread removes those traps. Before dyld publishes the library the resume
//!   breakpoints the executable entry and arms the pthread entries there, before
//!   that instruction runs. Failing to install a required trap fails the resume.
//!   While a held thread executes the restored entry, every other thread stays
//!   suspended. A software breakpoint on a pthread entry is reported, and the
//!   newborn stays suspended. `read_memory` shows the instructions under those
//!   traps, and an engine breakpoint written there survives their removal. A thread
//!   told to `Continue` is not held when it re-enters `_start_wqthread` as a reused
//!   workqueue thread: that entry instruction is emulated. On an x86-64 target, a
//!   default-`Suspend` resume that lets a thread run is `BackendError::Unsupported`.
//!   A suspended thread's stop record and any held Unix signal reply remain until a later
//!   resume lets it run; hardware exception replies are sent while the task is stopped. The next stop `wait` delivers ends that resume: the per-thread
//!   suspends are balanced and every single-step bit is cleared, so a step that did not
//!   complete is cancelled; `detach`, `kill` and drop balance them too. `Step` is not
//!   implemented on x86-64 (`BackendError::Unsupported`). `ThreadAction::Step` as the
//!   default, and threads whose start address is neither pthread entry, are outside the
//!   new-thread guarantee.
//! - **Exception ports**: attaching saves the target's exception ports and detaching
//!   restores them. The port rights each exception message carries are released.
//!   [`diagnostics`] lets tests check both.
//! - **Lifecycle**: PT_ATTACHEXC routes Unix signals through the exception port. The
//!   attach SIGSTOP is private; resume suppresses held signals unless explicitly passed.
//!   A kqueue watcher reaps exits with waitpid and reports Exited/Killed, including
//!   death while stopped. Kill terminates and reaps; detach uses PT_DETACH and restores
//!   the saved exception ports. Launched targets are killed on drop, attached ones released.
//! - **Thread ids** are Mach thread port names.
//! - **Breakpoints**: the backend keeps no breakpoint table; the engine writes trap
//!   bytes with `write_memory`, which makes code pages writable for the write. Hardware
//!   breakpoints and watchpoints are not part of the `Backend` interface; the unverified
//!   helpers in [`breakpoints`] program debug registers directly.
//! - **Images**: the main executable and dyld-loaded libraries are reported (see [`images`]).
//!
//! ## Known gaps
//!
//! - New-thread suspension covers arm64 `libsystem_pthread` entries only. Before
//!   that image is published, the executable entry arms them once dyld has mapped it.
//!
//! This is the only crate in the workspace that talks to an operating-system API
//! and the only one allowed to use `unsafe` (for raw Mach FFI); `ferros-core` holds
//! no OS-specific code. The whole crate is macOS-only: on any other OS it compiles
//! as an empty crate.
//!
//! macOS uses the Mach microkernel, which provides debugging capabilities
//! through Mach ports and messages. Unlike Linux's `ptrace`, macOS debugging
//! is based on:
//!
//! - **Mach tasks**: Represent a process
//! - **Mach threads**: Represent threads within a task
//! - **Mach ports**: Communication channels to tasks/threads
//!
//! ## Platform Requirements
//!
//! ### Minimum macOS Version
//!
//! - **macOS 10.15 (Catalina) or later**: `launch()` uses `POSIX_SPAWN_START_SUSPENDED`
//!   (macOS 10.5+) and `posix_spawn_file_actions_addchdir_np` (macOS 10.15+) for the
//!   working directory.
//!
//! ### Architecture Support
//!
//! - **ARM64 (Apple Silicon)**: Fully supported (M1, M2, M3, M4, etc.)
//!   - Primary target architecture
//!   - Uses `ARM_THREAD_STATE64` flavor for register access
//! - **x86_64 (Intel Mac)**: Supported for compatibility
//!   - Uses `X86_THREAD_STATE64` flavor for register access
//!   - Note: Intel Macs are no longer sold, but support is maintained
//!
//! Only arm64 is verified: the tests run on Apple Silicon. The x86_64 register code
//! (`registers/x86_64.rs`, compiled only for `target_arch = "x86_64"`) is carried
//! along from ferros-core untested.
//!
//! ## Permissions and Entitlements
//!
//! macOS requires special permissions to debug other processes. There are two approaches:
//!
//! ### Option 1: Launch Processes (Recommended)
//!
//! Use `launch()` instead of `attach()` to spawn processes under debugger control.
//! The process starts suspended, allowing breakpoints to be set before execution.
//! Taking control of it still calls `task_for_pid()`, so the same permission applies.
//!
//! ### Option 2: Attach to Running Processes
//!
//! To attach to already-running processes, you need debugging permissions:
//!
//! 1. **Run with sudo** (Simplest):
//!    ```bash
//!    sudo ferros attach <pid>
//!    ```
//!    - Quick and easy - no setup required
//!    - Works immediately without code signing
//!    - Requires password each time (or use `sudo` with NOPASSWD)
//!
//! 2. **Code signing with entitlements** (Advanced - optional):
//!    - Create `ferros.entitlements` file (see `crates/ferros-macos/ferros.entitlements`)
//!    - Sign the binary: `codesign --entitlements ferros.entitlements --force --sign - target/debug/ferros`
//    - The `com.apple.security.cs.debugger` entitlement grants debugging permissions
//!    - Useful if you want to avoid typing `sudo` repeatedly
//!
//! **Recommendation**: For most users, running with `sudo` is the simplest approach.
//! Entitlements are optional and mainly useful for development workflows where you
//! frequently attach to processes.
//!
//! ## Key Mach APIs Used
//!
//! - `task_for_pid()`: Get a Mach port to a process (declared ourselves - not in mach2)
//! - `task_threads()`: Enumerate threads in a task (from `mach2` crate)
//! - `thread_get_state()`: Read thread registers (declared ourselves - not in mach2)
//! - `thread_set_state()`: Write thread registers
//! - `task_suspend()` / `task_resume()`: Stop and continue the whole task
//! - `thread_suspend()` / `thread_resume()`: Keep single threads suspended during a resume
//! - `task_swap_exception_ports()` / `mach_msg()`: Receive exceptions (`task_set_exception_ports()`
//!   restores the saved ports on detach)
//! - `posix_spawn()`: Launch processes with `POSIX_SPAWN_START_SUSPENDED` flag
//!
//! ## Dependencies
//!
//! We use a hybrid approach:
//! - **mach2 crate**: For well-maintained Mach APIs (`mach_task_self`, `task_threads`, `KERN_SUCCESS`)
//! - **libc crate**: For type definitions (`mach_port_t`, `thread_act_t`, `posix_spawnattr_t`, etc.)
//! - **ffi module**: Centralized FFI declarations for restricted functions not in mach2
//!   (`task_for_pid`, `thread_get_state`, `vm_read`, `vm_write`, `mach_vm_region`, `posix_spawn`,
//!   `mach_port_names`, `mach_port_get_refs`)
//!
//! This gives us the best of both worlds: well-maintained APIs where available,
//! and direct control over restricted functions.
//!
//! ## References
//!
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - [XNU Kernel Source](https://github.com/apple-oss-distributions/xnu) (for `task_for_pid` implementation)
//! - [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
//! - [posix_spawn documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawn.3.html)
//! - [macOS Debugging Entitlements](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)

#![cfg(target_os = "macos")]
#![allow(unsafe_code)] // Required for raw Mach FFI
#![warn(missing_docs)]

mod birth;
pub mod breakpoints;
pub mod constants;
pub mod diagnostics;
pub mod error;
pub mod exception;
pub mod ffi;
pub mod guards;
pub mod images;
pub mod launch;
mod lifecycle;
pub mod memory;
pub mod registers;
pub mod task;
pub mod threads;
pub mod types;

pub use task::MacOSBackend;
