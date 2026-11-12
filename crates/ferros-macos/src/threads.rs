//! # macOS Thread Management
//!
//! Thread enumeration and management for macOS debugger.
//!
//! This module handles thread enumeration and per-thread suspend/resume
//! operations using Mach APIs. Thread selection belongs to the portable engine
//! ([`ferros_core::Session`]), not to the backend.
//!
//! ## Mach APIs Used
//!
//! - **task_threads()**: Enumerate threads in a task
//! - **thread_suspend()**: Suspend a single thread
//! - **thread_resume()**: Resume a single thread
//! - **mach_port_deallocate()**: Release thread ports
//! - **vm_deallocate()**: Free memory allocated by task_threads()
//!
//! ## References
//!
//! - [task_threads(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_threads/)
//! - [thread_suspend(3) man page](https://developer.apple.com/documentation/kernel/1418926-thread_suspend/)
//! - [thread_resume(3) man page](https://developer.apple.com/documentation/kernel/1418926-thread_resume/)

use std::mem;

use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::ThreadId;
use libc::{mach_msg_type_number_t, thread_act_t, vm_address_t, vm_size_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::task::task_threads;
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;

use crate::ffi;

/// Thread management functions for macOS debugger.
pub(crate) struct ThreadManager;

impl ThreadManager
{
    /// Deallocate memory allocated by `task_threads()`.
    ///
    /// This is an internal helper method that frees the memory allocated by the
    /// Mach API `task_threads()`. The Mach API allocates memory for the thread
    /// array that must be freed using `vm_deallocate()`.
    ///
    /// ## Parameters
    ///
    /// - `threads`: Pointer to the thread array allocated by `task_threads()`
    /// - `count`: Number of threads in the array
    ///
    /// ## Safety
    ///
    /// This function is safe to call with any pointer and count. It checks for null
    /// pointers and zero counts before attempting to deallocate.
    ///
    /// ## Mach API
    ///
    /// Uses `vm_deallocate()` to free the memory:
    /// - See: [vm_deallocate documentation](https://developer.apple.com/documentation/kernel/1585284-vm_deallocate/)
    pub(crate) fn deallocate_threads_array(threads: *mut thread_act_t, count: mach_msg_type_number_t)
    {
        if threads.is_null() || count == 0 {
            return;
        }

        let size = (count as usize).saturating_mul(mem::size_of::<thread_act_t>()) as vm_size_t;
        unsafe {
            let _ = ffi::vm_deallocate(mach_task_self(), threads as vm_address_t, size);
        }
    }

    /// Enumerate the threads of `task` from the operating system.
    ///
    /// Returns the current set of thread ports. The ports in `previous` (the list
    /// returned by the last call, if any) are deallocated after the new list has been
    /// obtained, never before: a thread that still exists keeps its port name, so a
    /// [`ThreadId`] names the same thread across stops.
    ///
    /// ## Mach API: task_threads()
    ///
    /// ```c
    /// kern_return_t task_threads(
    ///     task_t target_task,           // Task port from task_for_pid()
    ///     thread_act_array_t *act_list, // Output: array of thread ports
    ///     mach_msg_type_number_t *count // Output: number of threads
    /// );
    /// ```
    ///
    /// **Returns**: Array of thread ports. The memory must be freed using `vm_deallocate()`.
    ///
    /// See: [task_threads documentation](https://developer.apple.com/documentation/kernel/1537751-task_threads/)
    ///
    /// ## Errors
    ///
    /// Returns `DebuggerError::Os` with the `kern_return_t` if `task_threads()` fails;
    /// `previous` is then left untouched.
    pub(crate) fn refresh_thread_list(task: libc::mach_port_t, previous: &[thread_act_t]) -> Result<Vec<thread_act_t>>
    {
        unsafe {
            let mut threads: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;
            let result = task_threads(task, &mut threads, &mut thread_count);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::Os {
                    code: i64::from(result),
                    message: format!("Failed to enumerate threads: {result}"),
                });
            }

            let list = if threads.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(threads, thread_count as usize).to_vec()
            };
            Self::deallocate_threads_array(threads, thread_count);

            // Release the previous rights only now, so names of live threads stay stable.
            for thread in previous {
                let _ = ffi::mach_port_deallocate(mach_task_self(), *thread);
            }

            Ok(list)
        }
    }

    /// Suspend a specific thread.
    ///
    /// This function suspends execution of a single thread within the target task.
    /// The thread will stop executing until `resume_thread()` is called.
    ///
    /// ## Mach API: thread_suspend()
    ///
    /// ```c
    /// kern_return_t thread_suspend(thread_act_t target_act);
    /// ```
    ///
    /// **Parameters**:
    /// - `target_act`: Thread port (from `task_threads()`) to suspend
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [thread_suspend documentation](https://developer.apple.com/documentation/kernel/1418926-thread_suspend/)
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Thread ID is not valid (not in the thread list)
    /// - `SuspendFailed`: `thread_suspend()` failed
    pub(crate) fn suspend_thread(threads: &[thread_act_t], thread_id: ThreadId) -> Result<()>
    {
        let thread_port = thread_id.raw() as thread_act_t;
        if !threads.contains(&thread_port) {
            return Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of the target process",
                thread_id.raw()
            )));
        }

        unsafe {
            let result = ffi::thread_suspend(thread_port);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::SuspendFailed(format!(
                    "thread_suspend failed for thread {}: {}",
                    thread_id.raw(),
                    result
                )));
            }
        }

        Ok(())
    }

    /// Resume a specific thread.
    ///
    /// This function resumes execution of a single thread within the target task.
    /// The thread will continue from where it was suspended.
    ///
    /// ## Mach API: thread_resume()
    ///
    /// ```c
    /// kern_return_t thread_resume(thread_act_t target_act);
    /// ```
    ///
    /// **Parameters**:
    /// - `target_act`: Thread port (from `task_threads()`) to resume
    ///
    /// **Returns**: `KERN_SUCCESS` (0) on success, error code otherwise.
    ///
    /// See: [thread_resume documentation](https://developer.apple.com/documentation/kernel/1418926-thread_resume/)
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Thread ID is not valid (not in the thread list)
    /// - `ResumeFailed`: `thread_resume()` failed
    pub(crate) fn resume_thread(threads: &[thread_act_t], thread_id: ThreadId) -> Result<()>
    {
        let thread_port = thread_id.raw() as thread_act_t;
        if !threads.contains(&thread_port) {
            return Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of the target process",
                thread_id.raw()
            )));
        }

        unsafe {
            let result = ffi::thread_resume(thread_port);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::ResumeFailed(format!(
                    "thread_resume failed for thread {}: {}",
                    thread_id.raw(),
                    result
                )));
            }
        }

        Ok(())
    }
}
