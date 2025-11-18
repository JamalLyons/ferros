//! Debugger event types and helpers.
//!
//! Higher layers (CLI, TUI, future protocol clients) consume these events to
//! react to asynchronous target state changes without polling `is_stopped()` /
//! `stop_reason()`. Platform backends publish events whenever the kernel
//! delivers an exception (Mach) or a wait result (`waitpid`, Windows debug
//! loop, etc.).

use std::sync::mpsc;

use crate::types::{StopReason, ThreadId};

/// Event emitted by a debugger backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerEvent
{
    /// Target stopped execution for a particular reason.
    TargetStopped
    {
        /// Reason reported by the kernel/OS.
        reason: StopReason,
        /// Thread responsible for the stop (if known).
        thread: Option<ThreadId>,
    },
    /// Target resumed execution.
    TargetResumed,
}

impl DebuggerEvent
{
    /// Human-readable description of the event.
    #[must_use]
    pub fn describe(&self) -> String
    {
        match self {
            Self::TargetStopped { reason, thread } => {
                let mut description = format_stop_reason(*reason);
                if let Some(thread_id) = thread {
                    description.push_str(&format!(" (thread {})", thread_id.raw()));
                }
                description
            }
            Self::TargetResumed => "Target resumed execution".to_string(),
        }
    }
}

/// Format a [`StopReason`] into a user-facing message.
#[must_use]
pub fn format_stop_reason(reason: StopReason) -> String
{
    match reason {
        StopReason::Running => "Process is running".to_string(),
        StopReason::Suspended => "Process is suspended".to_string(),
        StopReason::Signal(sig) => format!("Stopped by signal: {sig}"),
        StopReason::Breakpoint(addr) => format!("Hit breakpoint at 0x{addr:x}"),
        StopReason::Exited(code) => format!("Process exited with code: {code}"),
        StopReason::Unknown => "Stopped for unknown reason".to_string(),
    }
}

/// Sender side of the debugger event channel.
pub type DebuggerEventSender = mpsc::Sender<DebuggerEvent>;
/// Receiver side of the debugger event channel.
pub type DebuggerEventReceiver = mpsc::Receiver<DebuggerEvent>;

/// Create a new debugger event channel.
#[must_use]
pub fn event_channel() -> (DebuggerEventSender, DebuggerEventReceiver)
{
    mpsc::channel()
}
