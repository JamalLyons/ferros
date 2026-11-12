//! # macOS Hardware Breakpoints and Watchpoints
//!
//! Hardware breakpoint and data watchpoint helpers for the macOS backend.
//!
//! Software breakpoints are not handled here: the portable engine
//! ([`ferros_core::Session`]) owns the breakpoint table and plants trap instructions
//! through [`ferros_core::Backend::write_memory`]. The backend keeps no breakpoint table.
//!
//! Hardware breakpoints use CPU debug registers (DR0-DR7 on x86-64, DBGBVR/DBGBCR on
//! ARM64) and are limited to 4 on x86-64 and 16 on ARM64. They are not part of the
//! [`Backend`](ferros_core::Backend) interface yet. The functions below program the debug
//! registers of every thread they are given; the caller keeps track of the returned slot.
//! They are unverified: no test exercises them yet.
//!
//! ## References
//!
//! - [registers::debug] for the debug register implementation

use ferros_core::breakpoints::WatchpointAccess;
use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::Address;
use libc::thread_act_t;

use crate::registers;

/// Install a hardware breakpoint at `address` on every thread in `threads`.
///
/// Returns the debug register slot used. Hardware breakpoints are installed on all
/// threads so that every thread stops at the address.
///
/// ## Errors
///
/// - `DebuggerError::AttachFailed`: `threads` is empty
/// - Any error from programming a thread's debug registers (for example all slots in use)
pub fn install_hardware_breakpoint(threads: &[thread_act_t], address: Address) -> Result<u32>
{
    let mut used_slot = None;

    for &thread in threads {
        let slot = registers::set_hardware_breakpoint(thread, address)?;
        if let Some(s) = used_slot {
            if s != slot {
                tracing::warn!("Hardware breakpoint slots inconsistent across threads: {} vs {}", s, slot);
            }
        } else {
            used_slot = Some(slot);
        }
    }

    used_slot.ok_or_else(|| DebuggerError::AttachFailed("No threads available to set hardware breakpoint".into()))
}

/// Install a data watchpoint for `length` bytes at `address` on every thread in `threads`.
///
/// On macOS this is currently implemented for ARM64 (Apple Silicon) only; x86-64
/// returns an error. Returns the watchpoint slot used.
///
/// ## Errors
///
/// - `DebuggerError::AttachFailed`: `threads` is empty
/// - Any error from programming a thread's debug registers
pub fn install_watchpoint(threads: &[thread_act_t], address: Address, length: usize, access: WatchpointAccess)
-> Result<u32>
{
    let mut used_slot = None;

    for &thread in threads {
        let slot = registers::set_watchpoint(thread, address, length, access)?;
        if let Some(s) = used_slot {
            if s != slot {
                tracing::warn!("Watchpoint slots inconsistent across threads: {} vs {}", s, slot);
            }
        } else {
            used_slot = Some(slot);
        }
    }

    used_slot.ok_or_else(|| DebuggerError::AttachFailed("No threads available to set data watchpoint".into()))
}

/// Remove the hardware breakpoint in `slot` from every thread in `threads`.
///
/// Best effort: a failure on one thread is logged as a warning and the others are
/// still cleared.
pub fn remove_hardware_breakpoint(threads: &[thread_act_t], slot: u32)
{
    for &thread in threads {
        if let Err(e) = registers::clear_hardware_breakpoint(thread, slot) {
            tracing::warn!("Failed to clear hardware breakpoint on thread {}: {}", thread, e);
        }
    }
}

/// Remove the data watchpoint in `slot` from every thread in `threads`.
///
/// Best effort: a failure on one thread is logged as a warning and the others are
/// still cleared.
pub fn remove_watchpoint(threads: &[thread_act_t], slot: u32)
{
    for &thread in threads {
        if let Err(e) = registers::clear_watchpoint(thread, slot) {
            tracing::warn!("Failed to clear data watchpoint on thread {}: {}", thread, e);
        }
    }
}
