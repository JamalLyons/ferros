//! # Platform-Specific Implementations
//!
//! This module contains platform-specific debugging implementations.
//!
//! Each platform has its own submodule that implements the `Debugger` trait
//! using that platform's native debugging APIs:
//!
//! - **macOS**: Uses Mach APIs (`task_for_pid`, `thread_get_state`, etc.)
//!   - See: [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - **Linux**: Will use `ptrace` system call (future)
//!   - See: [ptrace(2) man page](https://man7.org/linux/man-pages/man2/ptrace.2.html)
//! - **Windows**: Will use Windows Debug API (future)
//!   - See: [Windows Debugging API](https://docs.microsoft.com/en-us/windows/win32/debug/debugging-functions)
//!
//! ## Why separate modules?
//!
//! - **Clean separation**: Platform-specific code is isolated
//! - **Conditional compilation**: Only compile code for the current platform
//! - **Easy to extend**: Adding a new platform is just adding a new module
//! - **Clear organization**: Easy to find platform-specific code

// Platform-specific debugging implementations
//
// Each platform has its own module:
// - macos: Uses Mach APIs (task_for_pid, thread_get_state, etc.)
// - linux: Will use ptrace (future)
// - windows: Will use Windows Debug API (future)

#[cfg(target_os = "macos")]
pub mod macos;

// Future platform modules:
// #[cfg(target_os = "linux")]
// pub mod linux;
//
// #[cfg(target_os = "windows")]
// pub mod windows;
