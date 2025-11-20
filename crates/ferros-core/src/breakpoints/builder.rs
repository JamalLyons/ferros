//! # Breakpoint Builder
//!
//! Builder pattern for configuring breakpoints before installation.
//!
//! This module provides a fluent API for configuring breakpoints with
//! advanced features like conditional breakpoints, hit counts, and
//! thread-specific breakpoints.

use crate::breakpoints::{BreakpointRequest, WatchpointAccess};
use crate::error::Result;
use crate::types::{Address, Registers, ThreadId};

/// Type alias for breakpoint condition functions.
///
/// A condition function takes a reference to the current register state
/// and returns `true` if the breakpoint should trigger, `false` otherwise.
pub type BreakpointCondition = Box<dyn Fn(&Registers) -> bool + Send + Sync>;

/// Builder for configuring breakpoints before installation.
///
/// This builder allows you to configure advanced breakpoint features
/// like conditions, hit counts, and thread-specific breakpoints.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::Debugger;
/// use ferros_core::breakpoints::builder::BreakpointBuilder;
/// use ferros_core::types::Address;
///
/// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
/// let bp_id = BreakpointBuilder::software(Address::from(0x1000))
///     .with_hit_count(5)  // Break after 5 hits
///     .with_condition(|regs| regs.pc.value() > 0x2000)  // Only break if PC > 0x2000
///     .install(&mut debugger)?;
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
pub struct BreakpointBuilder
{
    request: BreakpointRequest,
    hit_count: Option<u64>,
    condition: Option<BreakpointCondition>,
    thread_id: Option<ThreadId>,
    commands: Vec<String>,
}

impl std::fmt::Debug for BreakpointBuilder
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        f.debug_struct("BreakpointBuilder")
            .field("request", &self.request)
            .field("hit_count", &self.hit_count)
            .field("condition", &self.condition.is_some())
            .field("thread_id", &self.thread_id)
            .field("commands", &self.commands)
            .finish()
    }
}

impl BreakpointBuilder
{
    /// Create a builder for a software breakpoint.
    ///
    /// ## Parameters
    ///
    /// - `address`: The memory address where the breakpoint should be installed.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// let builder = BreakpointBuilder::software(Address::from(0x1000));
    /// ```
    pub fn software(address: Address) -> Self
    {
        Self {
            request: BreakpointRequest::Software { address },
            hit_count: None,
            condition: None,
            thread_id: None,
            commands: Vec::new(),
        }
    }

    /// Create a builder for a hardware breakpoint.
    ///
    /// ## Parameters
    ///
    /// - `address`: The memory address where the breakpoint should be installed.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// let builder = BreakpointBuilder::hardware(Address::from(0x1000));
    /// ```
    pub fn hardware(address: Address) -> Self
    {
        Self {
            request: BreakpointRequest::Hardware { address },
            hit_count: None,
            condition: None,
            thread_id: None,
            commands: Vec::new(),
        }
    }

    /// Create a builder for a watchpoint.
    ///
    /// ## Parameters
    ///
    /// - `address`: The memory address where the watchpoint should be installed.
    /// - `length`: The size of the memory region to watch (in bytes).
    /// - `access`: The type of access that should trigger the watchpoint.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::WatchpointAccess;
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// let builder = BreakpointBuilder::watchpoint(Address::from(0x1000), 8, WatchpointAccess::Write);
    /// ```
    pub fn watchpoint(address: Address, length: usize, access: WatchpointAccess) -> Self
    {
        Self {
            request: BreakpointRequest::Watchpoint { address, length, access },
            hit_count: None,
            condition: None,
            thread_id: None,
            commands: Vec::new(),
        }
    }

    /// Set the hit count threshold.
    ///
    /// The breakpoint will only trigger after it has been hit `count` times.
    /// This is useful for debugging loops or frequently-called functions.
    ///
    /// ## Parameters
    ///
    /// - `count`: The number of times the breakpoint must be hit before it triggers.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// let builder = BreakpointBuilder::software(Address::from(0x1000))
    ///     .with_hit_count(10);  // Break on the 10th hit
    /// ```
    pub fn with_hit_count(mut self, count: u64) -> Self
    {
        self.hit_count = Some(count);
        self
    }

    /// Set a condition that must be true for the breakpoint to trigger.
    ///
    /// The condition is evaluated each time the breakpoint is hit. If the
    /// condition returns `false`, the breakpoint is ignored and execution
    /// continues.
    ///
    /// ## Parameters
    ///
    /// - `condition`: A closure that takes the current register state and
    ///   returns `true` if the breakpoint should trigger.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// let builder = BreakpointBuilder::software(Address::from(0x1000)).with_condition(|regs| {
    ///     // Only break if R0 (first general register) is 42
    ///     regs.general.get(0).map(|&r| r == 42).unwrap_or(false)
    /// });
    /// ```
    ///
    /// ## Note
    ///
    /// Conditional breakpoints are not yet fully implemented. This method
    /// stores the condition but it may not be evaluated until full support
    /// is added.
    pub fn with_condition<F>(mut self, condition: F) -> Self
    where
        F: Fn(&crate::types::Registers) -> bool + Send + Sync + 'static,
    {
        self.condition = Some(Box::new(condition));
        self
    }

    /// Make the breakpoint thread-specific.
    ///
    /// The breakpoint will only trigger when hit by the specified thread.
    /// Other threads will ignore the breakpoint.
    ///
    /// ## Parameters
    ///
    /// - `thread_id`: The thread ID that should trigger this breakpoint.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::{Address, ThreadId};
    ///
    /// let thread_id = ThreadId::from(123);
    /// let builder = BreakpointBuilder::software(Address::from(0x1000)).for_thread(thread_id);
    /// ```
    ///
    /// ## Note
    ///
    /// Thread-specific breakpoints are not yet fully implemented. This method
    /// stores the thread ID but it may not be enforced until full support
    /// is added.
    pub fn for_thread(mut self, thread_id: ThreadId) -> Self
    {
        self.thread_id = Some(thread_id);
        self
    }

    /// Add a command to run when the breakpoint is hit.
    ///
    /// Commands are executed in order when the breakpoint triggers. This
    /// allows you to automate debugging tasks like printing variable values
    /// or modifying registers.
    ///
    /// ## Parameters
    ///
    /// - `command`: A command string to execute (e.g., "print $r0").
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// let builder = BreakpointBuilder::software(Address::from(0x1000))
    ///     .with_command("print $r0")
    ///     .with_command("continue");
    /// ```
    ///
    /// ## Note
    ///
    /// Breakpoint commands are not yet fully implemented. This method stores
    /// the command but it may not be executed until full support is added.
    pub fn with_command(mut self, command: impl Into<String>) -> Self
    {
        self.commands.push(command.into());
        self
    }

    /// Install the breakpoint using the configured options.
    ///
    /// This method creates the breakpoint request and installs it using the
    /// debugger's `add_breakpoint()` method.
    ///
    /// ## Parameters
    ///
    /// - `debugger`: A mutable reference to a debugger instance.
    ///
    /// ## Returns
    ///
    /// The `BreakpointId` of the newly installed breakpoint.
    ///
    /// ## Errors
    ///
    /// Returns an error if:
    /// - The debugger is not attached to a process
    /// - A breakpoint already exists at the address
    /// - Hardware breakpoint slots are exhausted (for hardware breakpoints)
    /// - The breakpoint request is invalid
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    /// use ferros_core::breakpoints::builder::BreakpointBuilder;
    /// use ferros_core::types::Address;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let bp_id = BreakpointBuilder::software(Address::from(0x1000))
    ///     .with_hit_count(5)
    ///     .install(&mut debugger)?;
    /// println!("Installed breakpoint with ID: {}", bp_id.raw());
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    pub fn install<D: crate::debugger::Debugger>(self, debugger: &mut D) -> Result<crate::breakpoints::BreakpointId>
    {
        // For now, we just install the basic breakpoint.
        // Advanced features (hit count, conditions, commands) will be
        // implemented when the breakpoint hit handler supports them.
        debugger.add_breakpoint(self.request)
    }

    /// Get the underlying breakpoint request.
    ///
    /// This is useful if you want to inspect the request before installing it,
    /// or if you need to pass it to a custom installation function.
    pub fn request(&self) -> &BreakpointRequest
    {
        &self.request
    }

    /// Get the configured hit count threshold, if any.
    pub fn hit_count(&self) -> Option<u64>
    {
        self.hit_count
    }

    /// Get the configured thread ID, if any.
    pub fn thread_id(&self) -> Option<ThreadId>
    {
        self.thread_id
    }

    /// Get the configured commands.
    pub fn commands(&self) -> &[String]
    {
        &self.commands
    }
}
