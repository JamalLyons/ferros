//! # RAII Guards for macOS Debugger Operations
//!
//! This module provides RAII guards that automatically restore state when dropped.
//! These guards ensure that operations like suspending threads or disabling breakpoints
//! are properly cleaned up even if an error occurs.
//!
//! ## Guards
//!
//! - **ThreadSuspendGuard**: Suspends a thread and automatically resumes it on drop
//! - **BreakpointRestoreGuard**: Temporarily disables a breakpoint and restores it on drop
//!
//! ## Example
//!
//! ```rust,no_run
//! use ferros_core::platform::macos::guards::ThreadSuspendGuard;
//! use ferros_core::platform::macos::MacOSDebugger;
//! use ferros_core::types::ThreadId;
//! use ferros_core::Debugger;
//!
//! # let mut debugger = MacOSDebugger::new()?;
//! # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
//! let threads = debugger.threads()?;
//! if let Some(thread_id) = threads.first() {
//!     // Get the thread port (internal API, simplified for example)
//!     let thread_port = thread_id.raw() as u32; // In real code, use debugger's internal method
//!     let _guard = ThreadSuspendGuard::new(thread_port)?;
//!     // Thread is suspended, safe to inspect
//!     let regs = debugger.read_registers_for(*thread_id)?;
//!     // Guard automatically resumes thread when dropped
//! }
//! # Ok::<(), ferros_core::error::DebuggerError>(())
//! ```

use libc::thread_act_t;

use crate::breakpoints::BreakpointId;
use crate::error::Result;
use crate::platform::macos::ffi;

/// RAII guard that suspends a thread and automatically resumes it when dropped.
///
/// This guard ensures that threads are properly resumed even if an error occurs
/// or the code panics. It's useful for temporarily suspending a thread to inspect
/// its state safely.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::platform::macos::guards::ThreadSuspendGuard;
/// use ferros_core::platform::macos::MacOSDebugger;
/// use ferros_core::types::ThreadId;
/// use ferros_core::Debugger;
///
/// # let mut debugger = MacOSDebugger::new()?;
/// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
/// let threads = debugger.threads()?;
/// if let Some(thread_id) = threads.first() {
///     // Get the thread port (internal API, simplified for example)
///     let thread_port = thread_id.raw() as u32; // In real code, use debugger's internal method
///     let _guard = ThreadSuspendGuard::new(thread_port)?;
///     // Thread is suspended, safe to inspect
///     let regs = debugger.read_registers_for(*thread_id)?;
///     // Guard automatically resumes thread when dropped
/// }
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
pub struct ThreadSuspendGuard
{
    thread_port: thread_act_t,
    active: bool,
}

impl ThreadSuspendGuard
{
    /// Create a new guard that suspends the specified thread.
    ///
    /// The thread will be automatically resumed when the guard is dropped.
    ///
    /// ## Parameters
    ///
    /// - `thread_port`: The Mach thread port to suspend
    ///
    /// ## Errors
    ///
    /// - `SuspendFailed`: Failed to suspend the thread
    pub fn new(thread_port: thread_act_t) -> Result<Self>
    {
        unsafe {
            let result = ffi::thread_suspend(thread_port);
            if result != mach2::kern_return::KERN_SUCCESS {
                return Err(crate::error::DebuggerError::SuspendFailed(format!(
                    "thread_suspend failed: {}",
                    result
                )));
            }
        }

        Ok(Self {
            thread_port,
            active: true,
        })
    }

    /// Manually resume the thread before the guard is dropped.
    ///
    /// This is useful if you want to resume the thread early. After calling
    /// this method, dropping the guard will be a no-op.
    pub fn resume(mut self) -> Result<()>
    {
        if self.active {
            unsafe {
                let result = ffi::thread_resume(self.thread_port);
                if result != mach2::kern_return::KERN_SUCCESS {
                    return Err(crate::error::DebuggerError::ResumeFailed(format!(
                        "thread_resume failed: {}",
                        result
                    )));
                }
            }
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for ThreadSuspendGuard
{
    fn drop(&mut self)
    {
        if self.active {
            // Best effort resume - ignore errors
            unsafe {
                let _ = ffi::thread_resume(self.thread_port);
            }
        }
    }
}

/// RAII guard that temporarily disables a breakpoint and restores it when dropped.
///
/// This guard ensures that breakpoints are properly restored even if an error occurs
/// or the code panics. It's useful for temporarily disabling a breakpoint to execute
/// code without hitting it.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::breakpoints::BreakpointRequest;
/// use ferros_core::platform::macos::guards::BreakpointRestoreGuard;
/// use ferros_core::platform::macos::MacOSDebugger;
/// use ferros_core::types::Address;
/// use ferros_core::Debugger;
///
/// # let mut debugger = MacOSDebugger::new()?;
/// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
/// let bp_id = debugger.add_breakpoint(BreakpointRequest::Software {
///     address: Address::from(0x1000),
/// })?;
/// {
///     let _guard = BreakpointRestoreGuard::new(&mut debugger, bp_id)?;
///     // Breakpoint is disabled, code can execute without hitting it
///     // Note: Can't call debugger methods while guard holds mutable reference
///     // Guard automatically re-enables breakpoint when dropped
/// }
/// // Now safe to use debugger again
/// debugger.resume()?;
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
pub struct BreakpointRestoreGuard<'a>
{
    debugger: &'a mut dyn crate::debugger::Debugger,
    breakpoint_id: BreakpointId,
    was_enabled: bool,
    active: bool,
}

impl<'a> BreakpointRestoreGuard<'a>
{
    /// Create a new guard that disables the specified breakpoint.
    ///
    /// The breakpoint will be automatically re-enabled when the guard is dropped.
    ///
    /// ## Errors
    ///
    /// - `BreakpointIdNotFound`: Breakpoint doesn't exist
    /// - `InvalidArgument`: Failed to disable the breakpoint
    pub fn new(debugger: &'a mut dyn crate::debugger::Debugger, breakpoint_id: BreakpointId) -> Result<Self>
    {
        let was_enabled = debugger.breakpoint_info(breakpoint_id)?.enabled;
        if was_enabled {
            debugger.disable_breakpoint(breakpoint_id)?;
        }

        Ok(Self {
            debugger,
            breakpoint_id,
            was_enabled,
            active: true,
        })
    }

    /// Manually restore the breakpoint before the guard is dropped.
    ///
    /// This is useful if you want to restore the breakpoint early. After calling
    /// this method, dropping the guard will be a no-op.
    pub fn restore(mut self) -> Result<()>
    {
        if self.active && self.was_enabled {
            self.debugger.enable_breakpoint(self.breakpoint_id)?;
            self.active = false;
        }
        Ok(())
    }
}

impl<'a> Drop for BreakpointRestoreGuard<'a>
{
    fn drop(&mut self)
    {
        if self.active && self.was_enabled {
            // Best effort restore - ignore errors
            let _ = self.debugger.enable_breakpoint(self.breakpoint_id);
        }
    }
}
