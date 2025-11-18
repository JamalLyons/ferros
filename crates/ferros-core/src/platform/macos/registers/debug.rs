//! # macOS Debug Register Manipulation
//!
//! Functions to read/write CPU debug registers for hardware breakpoints.
//!
//! This module provides low-level access to CPU debug registers used for hardware
//! breakpoints. Unlike general-purpose registers (see [arm64](arm64) and [x86_64](x86_64)),
//! debug registers are special CPU registers dedicated to breakpoint and watchpoint control.
//!
//! ## Flavors
//!
//! - **x86-64**: `x86_DEBUG_STATE64` (flavor 11) - Uses DR0-DR7 registers
//! - **ARM64**: `ARM_DEBUG_STATE64` (flavor 15) - Uses DBGBVR/DBGBCR registers
//!
//! ## See Also
//!
//! - [arm64](arm64) - General-purpose ARM64 register access
//! - [x86_64](x86_64) - General-purpose x86-64 register access
//! - [breakpoints](../breakpoints) - High-level breakpoint management

use libc::{natural_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::{constants, ffi};
use crate::types::Address;

// Use constants from the centralized constants module

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct X86DebugState64
{
    dr0: u64,
    dr1: u64,
    dr2: u64,
    dr3: u64,
    dr4: u64,
    dr5: u64,
    dr6: u64,
    dr7: u64,
}

// Use constants from the centralized constants module

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct ArmDebugState64
{
    bvr: [u64; 16],
    bcr: [u64; 16],
    wvr: [u64; 16],
    wcr: [u64; 16],
    mdscr_el1: u64,
}

/// Set a hardware breakpoint at the given address.
///
/// Hardware breakpoints use CPU debug registers (DR0-DR7 on x86-64, DBGBVR/DBGBCR on ARM64)
/// to break on instruction execution without modifying code. This is useful for:
///
/// - Read-only code sections
/// - Self-modifying code
/// - Performance-critical paths
///
/// ## Limitations
///
/// - **x86-64**: Only 4 hardware breakpoints available (DR0-DR3)
/// - **ARM64**: Up to 16 hardware breakpoints available (DBGBVR0-15)
///
/// ## Mach APIs Used
///
/// - **thread_get_state()**: Reads current debug register state
/// - **thread_set_state()**: Writes updated debug register state
///
/// ## Thread State Flavors
///
/// - **x86-64**: `x86_DEBUG_STATE64` (flavor 11)
/// - **ARM64**: `ARM_DEBUG_STATE64` (flavor 15)
///
/// ## Returns
///
/// The slot index used (0-3 for x86-64, 0-15 for ARM64).
///
/// ## Errors
///
/// Returns `MachError` if `thread_get_state()` or `thread_set_state()` fails.
/// Returns `InvalidArgument` if all hardware breakpoint slots are in use.
///
/// ## See Also
///
/// - `clear_hardware_breakpoint()`: Removes a hardware breakpoint
/// - [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
/// - [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_set_state/)
pub fn set_hardware_breakpoint(thread: thread_act_t, address: Address) -> Result<u32>
{
    #[cfg(target_arch = "x86_64")]
    {
        set_hw_bp_x86(thread, address)
    }
    #[cfg(target_arch = "aarch64")]
    {
        set_hw_bp_arm64(thread, address)
    }
}

/// Clear a hardware breakpoint from the given slot.
///
/// This disables the hardware breakpoint in the specified slot by clearing
/// the corresponding enable bit in the debug control register.
///
/// ## Mach APIs Used
///
/// - **thread_get_state()**: Reads current debug register state
/// - **thread_set_state()**: Writes updated debug register state
///
/// ## Parameters
///
/// - `thread`: The thread port to modify
/// - `slot`: The slot index (0-3 for x86-64, 0-15 for ARM64)
///
/// ## Errors
///
/// Returns `MachError` if `thread_get_state()` or `thread_set_state()` fails.
/// Returns `InvalidArgument` if the slot index is out of range.
///
/// ## See Also
///
/// - `set_hardware_breakpoint()`: Sets a hardware breakpoint
/// - [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
/// - [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_set_state/)
pub fn clear_hardware_breakpoint(thread: thread_act_t, slot: u32) -> Result<()>
{
    #[cfg(target_arch = "x86_64")]
    {
        clear_hw_bp_x86(thread, slot)
    }
    #[cfg(target_arch = "aarch64")]
    {
        clear_hw_bp_arm64(thread, slot)
    }
}

#[cfg(target_arch = "x86_64")]
fn set_hw_bp_x86(thread: thread_act_t, address: Address) -> Result<u32>
{
    unsafe {
        let mut state = X86DebugState64::default();
        let mut count = constants::X86_DEBUG_STATE64_COUNT;
        let kr = ffi::thread_get_state(
            thread,
            constants::X86_DEBUG_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        // Find a free slot
        let slot = if (state.dr7 & (1 << 0)) == 0 {
            0
        } else if (state.dr7 & (1 << 2)) == 0 {
            1
        } else if (state.dr7 & (1 << 4)) == 0 {
            2
        } else if (state.dr7 & (1 << 6)) == 0 {
            3
        } else {
            return Err(DebuggerError::ResourceExhausted(
                "No free hardware breakpoint slots (maximum 4 on x86-64)".into(),
            ));
        };

        // Set address in appropriate DR register
        match slot {
            0 => state.dr0 = address.value(),
            1 => state.dr1 = address.value(),
            2 => state.dr2 = address.value(),
            3 => state.dr3 = address.value(),
            _ => unreachable!(),
        }

        // Update DR7 to enable the breakpoint
        // Bits 0, 2, 4, 6 are local enable (L0-L3)
        // Bits 1, 3, 5, 7 are global enable (G0-G3) - we use local for now
        state.dr7 |= 1 << (slot * 2);

        // Set RWn (Read/Write) and LENn (Length) fields
        // RWn: Bits 16-17, 20-21, 24-25, 28-29. 00 = Execute
        // LENn: Bits 18-19, 22-23, 26-27, 30-31. 00 = 1 byte (for execute)

        // Clear RWn and LENn bits for this slot first
        let rw_len_shift = 16 + (slot * 4);
        state.dr7 &= !(0xF << rw_len_shift);

        // For execution, we want RW=00 (execute) and LEN=00 (1 byte)
        // So we just leave them cleared.

        let kr = ffi::thread_set_state(
            thread,
            constants::X86_DEBUG_STATE64,
            &state as *const _ as *const natural_t,
            constants::X86_DEBUG_STATE64_COUNT,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(slot)
    }
}

#[cfg(target_arch = "x86_64")]
fn clear_hw_bp_x86(thread: thread_act_t, slot: u32) -> Result<()>
{
    unsafe {
        let mut state = X86DebugState64::default();
        let mut count = constants::X86_DEBUG_STATE64_COUNT;
        let kr = ffi::thread_get_state(
            thread,
            constants::X86_DEBUG_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        // Clear local enable bit
        state.dr7 &= !(1 << (slot * 2));

        let kr = ffi::thread_set_state(
            thread,
            constants::X86_DEBUG_STATE64,
            &state as *const _ as *const natural_t,
            constants::X86_DEBUG_STATE64_COUNT,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
fn set_hw_bp_arm64(thread: thread_act_t, address: Address) -> Result<u32>
{
    unsafe {
        let mut state = ArmDebugState64::default();
        let mut count = constants::ARM_DEBUG_STATE64_COUNT;
        let kr = ffi::thread_get_state(
            thread,
            constants::ARM_DEBUG_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        // Find a free slot
        // BCR (Breakpoint Control Register) bit 0 is enable (E)
        let mut slot = None;
        for i in 0..16 {
            if (state.bcr[i] & 1) == 0 {
                slot = Some(i);
                break;
            }
        }

        let slot = slot.ok_or_else(|| {
            DebuggerError::ResourceExhausted("No free hardware breakpoint slots (maximum 16 on ARM64)".into())
        })?;

        // Set BVR (Breakpoint Value Register) to address
        state.bvr[slot] = address.value();

        // Set BCR (Breakpoint Control Register)
        // Bit 0: E (Enable) = 1
        // Bits 1-2: PMC (Privilege Mode Control) = 0b11 (EL0 and EL1) - usually 0b11 or 0b10 (EL0 only) for user
        // Bits 5-8: BAS (Byte Address Select) = 0b1111 (match any byte)
        //
        // Standard value for user execution breakpoint:
        // E=1, PMC=10 (User), BAS=1111
        // 0b ... 1111 010 1
        // 0x1E5

        // BAS (Byte Address Select) is bits 5-8.
        // PMC (Privilege Mode Control) is bits 1-2.
        // E (Enable) is bit 0.

        // Let's use 0x1E5:
        // E = 1
        // PMC = 10 (PL0 - User)
        // BAS = 1111 (Match all bytes)
        // Note: The exact value might depend on kernel enforcement, but 0x1E5 is common.

        state.bcr[slot] = constants::ARM64_BP_CTRL_USER_EXEC;

        let kr = ffi::thread_set_state(
            thread,
            constants::ARM_DEBUG_STATE64,
            &state as *const _ as *const natural_t,
            constants::ARM_DEBUG_STATE64_COUNT,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(slot as u32)
    }
}

#[cfg(target_arch = "aarch64")]
fn clear_hw_bp_arm64(thread: thread_act_t, slot: u32) -> Result<()>
{
    unsafe {
        let mut state = ArmDebugState64::default();
        let mut count = constants::ARM_DEBUG_STATE64_COUNT;
        let kr = ffi::thread_get_state(
            thread,
            constants::ARM_DEBUG_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        if slot >= 16 {
            return Err(DebuggerError::InvalidArgument("Invalid breakpoint slot".into()));
        }

        // Clear enable bit (bit 0)
        state.bcr[slot as usize] = 0;

        let kr = ffi::thread_set_state(
            thread,
            constants::ARM_DEBUG_STATE64,
            &state as *const _ as *const natural_t,
            constants::ARM_DEBUG_STATE64_COUNT,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(())
    }
}
