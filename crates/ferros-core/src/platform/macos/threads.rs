//! # macOS Thread Management
//!
//! Thread enumeration and management for macOS debugger.
//!
//! This module handles thread enumeration, active thread selection, and
//! per-thread suspend/resume operations using Mach APIs.
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

use libc::{mach_msg_type_number_t, thread_act_t, vm_address_t, vm_size_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::task::task_threads;
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::ffi;
use crate::types::ThreadId;

/// Trait for thread operations that require access to debugger internals.
pub(crate) trait ThreadOperations
{
    /// Get the task port.
    fn task_port(&self) -> libc::mach_port_t;

    /// Get the list of thread ports (mutable).
    fn thread_ports_mut(&mut self) -> &mut Vec<thread_act_t>;

    /// Get the list of thread ports (immutable).
    fn thread_ports(&self) -> &[thread_act_t];

    /// Get the current active thread port.
    fn current_thread(&self) -> Option<thread_act_t>;

    /// Set the current active thread port.
    fn set_current_thread(&mut self, thread: Option<thread_act_t>);

    /// Get the process ID (for error messages).
    fn pid(&self) -> u32;
}

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

    /// Refresh the thread list from the operating system.
    ///
    /// This is an internal helper method that updates the cached thread list by
    /// calling `task_threads()` to get the current set of threads in the target
    /// process. It also updates the active thread if the current one no longer exists.
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
    /// ## Implementation Notes
    ///
    /// - Deallocates old thread ports before getting new ones to prevent port leaks
    /// - Updates the active thread to the first thread if the current one no longer exists
    ///
    /// ## Mach APIs Used
    ///
    /// - **task_threads()**: Enumerates all threads in the task
    /// - **mach_port_deallocate()**: Releases old thread ports
    ///
    /// ## Errors
    ///
    /// Returns `AttachFailed` if `task_threads()` fails or no threads found.
    ///
    /// ## See Also
    ///
    /// - [task_threads(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_threads/)
    pub(crate) fn refresh_thread_list<Ops: ThreadOperations>(ops: &mut Ops) -> Result<()>
    {
        unsafe {
            // Deallocate old thread ports before getting new ones to prevent port leaks
            for thread in ops.thread_ports() {
                let _ = ffi::mach_port_deallocate(mach_task_self(), *thread);
            }

            let mut threads: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;
            let result = task_threads(ops.task_port(), &mut threads, &mut thread_count);
            if result != KERN_SUCCESS {
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to enumerate threads: {}",
                    result
                )));
            }

            let slice = std::slice::from_raw_parts(threads, thread_count as usize);
            *ops.thread_ports_mut() = slice.to_vec();
            Self::deallocate_threads_array(threads, thread_count);

            // Update active thread - use first thread if current one no longer exists
            if let Some(current) = ops.current_thread() {
                if !ops.thread_ports().contains(&current) {
                    ops.set_current_thread(ops.thread_ports().first().copied());
                }
            } else {
                ops.set_current_thread(ops.thread_ports().first().copied());
            }
        }

        Ok(())
    }

    /// Get the active thread port.
    pub(crate) fn active_thread_port<Ops: ThreadOperations>(ops: &Ops) -> Result<thread_act_t>
    {
        ops.current_thread()
            .ok_or_else(|| DebuggerError::InvalidArgument("No active thread selected".to_string()))
    }

    /// Get the thread port for a given ThreadId.
    pub(crate) fn thread_port_for_id<Ops: ThreadOperations>(ops: &Ops, thread: ThreadId) -> Result<thread_act_t>
    {
        let port = thread.raw() as thread_act_t;
        if ops.thread_ports().contains(&port) {
            Ok(port)
        } else {
            Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of process {}. Call refresh_threads() to update the thread list.",
                thread.raw(),
                ops.pid()
            )))
        }
    }

    /// Set the active thread using a Mach thread port.
    ///
    /// This is an internal helper method that sets the active thread using a raw
    /// `thread_act_t` (Mach thread port). It validates that the thread belongs
    /// to the current process before setting it as active.
    ///
    /// ## Parameters
    ///
    /// - `port`: The Mach thread port (`thread_act_t`) to make active
    ///
    /// ## Errors
    ///
    /// Returns `InvalidArgument` if the thread port is not in the current thread list.
    pub(crate) fn set_active_thread_by_port<Ops: ThreadOperations>(ops: &mut Ops, port: thread_act_t) -> Result<()>
    {
        if ops.thread_ports().contains(&port) {
            ops.set_current_thread(Some(port));
            Ok(())
        } else {
            Err(DebuggerError::InvalidArgument(format!(
                "Thread {port} is not part of process {}",
                ops.pid()
            )))
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
    pub(crate) fn suspend_thread<Ops: ThreadOperations>(ops: &Ops, thread_id: ThreadId) -> Result<()>
    {
        let thread_port = thread_id.raw() as thread_act_t;
        if !ops.thread_ports().contains(&thread_port) {
            return Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of process {}",
                thread_id.raw(),
                ops.pid()
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
    pub(crate) fn resume_thread<Ops: ThreadOperations>(ops: &Ops, thread_id: ThreadId) -> Result<()>
    {
        let thread_port = thread_id.raw() as thread_act_t;
        if !ops.thread_ports().contains(&thread_port) {
            return Err(DebuggerError::InvalidArgument(format!(
                "Thread {} is not part of process {}",
                thread_id.raw(),
                ops.pid()
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
