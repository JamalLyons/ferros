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
//! ## Key Mach APIs Used
//!
//! - `task_for_pid()`: Get a Mach port to a process (declared ourselves - not in mach2)
//! - `task_threads()`: Enumerate threads in a task (from `mach2` crate)
//! - `thread_get_state()`: Read thread registers (declared ourselves - not in mach2)
//! - `thread_set_state()`: Write thread registers (future)
//!
//! ## Dependencies
//!
//! We use a hybrid approach:
//! - **mach2 crate**: For well-maintained Mach APIs (`mach_task_self`, `task_threads`, `KERN_SUCCESS`)
//! - **libc crate**: For type definitions (`mach_port_t`, `thread_act_t`, etc.)
//! - **extern "C"**: For restricted functions not in mach2 (`task_for_pid`, `thread_get_state`)
//!
//! This gives us the best of both worlds: well-maintained APIs where available,
//! and direct control over restricted functions.
//!
//! ## References
//!
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - [task_for_pid(3) man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/task_for_pid.3.html)
//! - [thread_get_state(3) man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man3/thread_get_state.3.html)

pub mod error;
pub mod registers;
pub mod task;

pub use task::MacOSDebugger;
