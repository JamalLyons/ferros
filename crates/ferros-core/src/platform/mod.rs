//! # Platform-Specific Implementations
//!
//! This module contains platform-specific debugging implementations.
//!
//! Each platform has its own submodule that implements the `FerrosDebugger` trait
//! using that platform's native debugging APIs:
//!
//! - **macOS**: Uses Mach APIs (`task_for_pid`, `thread_get_state`, etc.)
//!   - See: [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - **Linux**: TBD
//! - **Windows**: TBD

#[cfg(target_os = "macos")]
pub mod macos;
