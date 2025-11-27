//! # macOS Debugging Implementation
//!
//! macOS-specific debugger implementation using Mach APIs.
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
//! - **macOS 10.5 (Leopard) or later**: Required for `POSIX_SPAWN_START_SUSPENDED` flag
//!   used in `launch()`. This flag allows spawning processes in a suspended state.
//! - **Recommended: macOS 10.15 (Catalina) or later**: For best compatibility with
//!   modern Rust toolchains and development tools.
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
//! ## Permissions and Entitlements
//!
//! macOS requires special permissions to debug other processes. There are two approaches:
//!
//! ### Option 1: Launch Processes (Recommended)
//!
//! Use `launch()` instead of `attach()` to spawn processes under debugger control.
//! This avoids permission issues because:
//! - Launching processes doesn't require special permissions
//! - The debugger automatically gets control of processes it launches
//! - Process starts suspended, allowing breakpoints to be set before execution
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
//!    - Create `ferros.entitlements` file (see `crates/ferros-core/ferros.entitlements`)
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
//! - `thread_set_state()`: Write thread registers (future)
//! - `posix_spawn()`: Launch processes with `POSIX_SPAWN_START_SUSPENDED` flag
//!
//! ## Dependencies
//!
//! We use a hybrid approach:
//! - **mach2 crate**: For well-maintained Mach APIs (`mach_task_self`, `task_threads`, `KERN_SUCCESS`)
//! - **libc crate**: For type definitions (`mach_port_t`, `thread_act_t`, `posix_spawnattr_t`, etc.)
//! - **ffi module**: Centralized FFI declarations for restricted functions not in mach2
//!   (`task_for_pid`, `thread_get_state`, `vm_read`, `vm_write`, `mach_vm_region`, `posix_spawn`)
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

pub mod error;
