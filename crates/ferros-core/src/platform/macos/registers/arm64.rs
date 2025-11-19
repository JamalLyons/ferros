//! # ARM64 Register Reading and Writing
//!
//! Functions to read and write ARM64 CPU registers from macOS processes using Mach APIs.
//!
//! ## ARM64 Register Layout
//!
//! ARM64 has 31 general-purpose registers (X0-X30) plus special registers:
//!
//! - **X0-X28**: General-purpose registers (29 registers)
//! - **X29 (FP)**: Frame pointer
//! - **X30 (LR)**: Link register (return address)
//! - **SP**: Stack pointer
//! - **PC**: Program counter
//! - **CPSR**: Current Program Status Register (flags)
//!
//! ## Thread State Structure
//!
//! macOS stores ARM64 thread state as an array of 68 `u32` values:
//!
//! ```rs
//! Index 0-28:   X0-X28 (29 registers × 2 u32s = 58 u32s)
//! Index 29:     FP (X29) (2 u32s)
//! Index 30:     LR (X30) (2 u32s)
//! Index 31:     SP (2 u32s)
//! Index 32:     PC (2 u32s)
//! Index 33:     CPSR + padding (2 u32s)
//! Total: 34 u64s = 68 u32s
//! ```
//!
//! ## References
//!
//! - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
//! - [ARM CPSR Register](https://developer.arm.com/documentation/dui0801/a/A32-and-T32-Instructions/CPSR)
//! - [ARM_THREAD_STATE64 structure](https://opensource.apple.com/source/xnu/xnu-4570.71.2/osfmk/mach/arm/_structs.h)

use libc::{mach_msg_type_number_t, natural_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::{KERN_INVALID_ARGUMENT, KERN_SUCCESS};
use tracing::debug;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::{constants, ffi};
use crate::types::{Address, Architecture, Registers, VectorRegisterValue};

/// Read ARM64 registers from a thread
///
/// This function reads the CPU registers from an ARM64 thread using the
/// `thread_get_state()` Mach API with the `ARM_THREAD_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to read registers from
///
/// ## Returns
///
/// `Ok(Registers)` containing all register values, or an error if:
/// - `thread_get_state()` failed
/// - NEON state read failed (non-fatal, logged as debug)
///
/// ## Registers Read
///
/// - General-purpose registers: X0-X30 (31 registers)
/// - Stack pointer (SP)
/// - Frame pointer (FP/X29)
/// - Program counter (PC)
/// - Current Program Status Register (CPSR)
/// - NEON/SIMD registers: V0-V31 (if available)
/// - Floating-point status registers: FPSR, FPCR (if available)
///
/// ## Mach API: thread_get_state()
///
/// **Flavor**: `ARM_THREAD_STATE64` = 6
/// **Count**: `ARM_THREAD_STATE64_COUNT` = 68 (number of `u32` values)
///
/// ## Errors
///
/// - `DebuggerError::ReadRegistersFailed`: `thread_get_state()` failed
///
/// ## See Also
///
/// - [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
pub fn read_registers_arm64(thread: thread_act_t) -> Result<Registers>
{
    unsafe {
        // Allocate array to hold thread state
        // macOS returns registers as an array of natural_t (u32)
        // We need 68 u32s to hold all ARM64 registers
        let mut state_words: [natural_t; constants::ARM_THREAD_STATE64_COUNT as usize] =
            [0; constants::ARM_THREAD_STATE64_COUNT as usize];
        let mut count: mach_msg_type_number_t = constants::ARM_THREAD_STATE64_COUNT;

        // Call thread_get_state to read registers
        let result = ffi::thread_get_state(thread, constants::ARM_THREAD_STATE64, state_words.as_mut_ptr(), &mut count);

        // Check if the call succeeded
        if result != KERN_SUCCESS {
            return Err(DebuggerError::ReadRegistersFailed {
                operation: "read ARM64 thread state".to_string(),
                thread_id: None,
                details: format!("thread_get_state failed: {}", result),
            });
        }

        // Helper function to read a u64 from two u32s
        // macOS stores 64-bit registers as two 32-bit values in little-endian format:
        // - Low 32 bits at index i * 2
        // - High 32 bits at index i * 2 + 1
        let read_u64 = |idx: usize| -> u64 {
            let low = state_words[idx * 2];
            let high = state_words[idx * 2 + 1];
            (low as u64) | ((high as u64) << 32)
        };

        // Parse the register values from the state array
        let mut regs = Registers::new();
        regs.set_architecture(Architecture::Arm64);

        // Program Counter: Points to the next instruction to execute
        regs.pc = Address::from(read_u64(constants::ARM64_PC_INDEX));

        // Stack Pointer: Points to the top of the stack
        regs.sp = Address::from(read_u64(constants::ARM64_SP_INDEX));

        // Frame Pointer: Points to the current stack frame
        regs.fp = Address::from(read_u64(constants::ARM64_FP_INDEX));

        // CPSR (Current Program Status Register): Contains flags
        regs.status = state_words[constants::ARM64_CPSR_INDEX] as u64;

        // General-purpose registers: X0-X30
        regs.general = (0..=30).map(read_u64).collect();

        if let Some(neon) = fetch_arm64_neon_state(thread)? {
            regs.vector = neon.v.iter().map(|&value| VectorRegisterValue::from_u128(value)).collect();
            regs.floating.fpsr = Some(neon.fpsr);
            regs.floating.fpcr = Some(neon.fpcr);
        }

        Ok(regs)
    }
}

/// Write ARM64 registers to a thread
///
/// This function writes CPU registers to an ARM64 thread using the
/// `thread_set_state()` Mach API with the `ARM_THREAD_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to write registers to
/// - `regs`: The `Registers` structure containing the register values to write
///
/// ## Mach API: thread_set_state()
///
/// **Flavor**: `ARM_THREAD_STATE64` = 6
/// **Count**: `ARM_THREAD_STATE64_COUNT` = 68 (number of `u32` values)
///
/// ## Registers Written
///
/// - General-purpose registers: X0-X30
/// - Stack pointer (SP)
/// - Frame pointer (FP/X29)
/// - Program counter (PC)
/// - Current Program Status Register (CPSR)
/// - NEON/SIMD registers (if available)
/// - Floating-point status registers (FPSR, FPCR)
///
/// ## Returns
///
/// `Ok(())` if the registers were successfully written, or an error if:
/// - `thread_set_state()` failed
/// - NEON state is not available (if attempting to write vector registers)
///
/// ## Errors
///
/// - `DebuggerError::InvalidArgument`: `thread_set_state()` failed or NEON state unavailable
///
/// ## See Also
///
/// - [thread_set_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_set_state/)
pub fn write_registers_arm64(thread: thread_act_t, regs: &Registers) -> Result<()>
{
    let mut state_words: [natural_t; constants::ARM_THREAD_STATE64_COUNT as usize] =
        [0; constants::ARM_THREAD_STATE64_COUNT as usize];

    let mut write_u64 = |idx: usize, value: u64| {
        state_words[idx * 2] = (value & constants::U32_MASK) as natural_t;
        state_words[idx * 2 + 1] = (value >> 32) as natural_t;
    };

    // General-purpose registers X0-X30
    for i in 0..=30 {
        let mut value = regs.general.get(i).copied().unwrap_or(0);
        if i == constants::ARM64_FP_INDEX {
            value = regs.fp.value();
        }
        write_u64(i, value);
    }

    // SP and PC
    write_u64(constants::ARM64_SP_INDEX, regs.sp.value());
    write_u64(constants::ARM64_PC_INDEX, regs.pc.value());

    // Status/CPSR (single u32)
    state_words[constants::ARM64_CPSR_INDEX] = (regs.status & constants::U32_MASK) as natural_t;

    unsafe {
        let result = ffi::thread_set_state(
            thread,
            constants::ARM_THREAD_STATE64,
            state_words.as_ptr(),
            constants::ARM_THREAD_STATE64_COUNT,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::InvalidArgument(format!("thread_set_state failed: {}", result)));
        }
    }

    write_arm64_neon_state(thread, regs)?;

    Ok(())
}

/// ARM64 NEON/SIMD state structure.
///
/// This structure represents the NEON (Advanced SIMD) register state for ARM64,
/// including 32 128-bit vector registers (V0-V31) and floating-point status/control
/// registers (FPSR, FPCR).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ArmNeonState64
{
    /// Vector registers V0-V31 (32 × 128-bit = 32 × u128)
    v: [u128; 32],
    /// Floating-Point Status Register (FPSR)
    fpsr: u32,
    /// Floating-Point Control Register (FPCR)
    fpcr: u32,
}

/// Fetch ARM64 NEON/SIMD state from a thread.
///
/// This function reads the NEON vector registers and floating-point status/control
/// registers from an ARM64 thread using the `ARM_NEON_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to read from
///
/// ## Returns
///
/// `Ok(Some(state))` if NEON state is available and successfully read,
/// `Ok(None)` if NEON state is not available on this system,
/// or an error if `thread_get_state()` failed for other reasons.
///
/// ## Mach API: thread_get_state()
///
/// **Flavor**: `ARM_NEON_STATE64` = 5
/// **Count**: `ARM_NEON_STATE64_COUNT` = 68
///
/// ## Note
///
/// NEON state may not be available on all ARM64 systems. This function gracefully
/// handles the case where NEON is not supported by returning `Ok(None)`.
fn fetch_arm64_neon_state(thread: thread_act_t) -> Result<Option<ArmNeonState64>>
{
    let mut state = ArmNeonState64::default();
    let mut count = constants::ARM_NEON_STATE64_COUNT;
    let kr = unsafe {
        ffi::thread_get_state(
            thread,
            constants::ARM_NEON_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        )
    };

    if kr == KERN_SUCCESS {
        Ok(Some(state))
    } else if kr == KERN_INVALID_ARGUMENT {
        debug!("ARM NEON state not available on this system");
        Ok(None)
    } else {
        Err(DebuggerError::ReadRegistersFailed {
            operation: "read ARM64 NEON state".to_string(),
            thread_id: None,
            details: format!("thread_get_state(ARM_NEON_STATE64) failed: {}", kr),
        })
    }
}

/// Write ARM64 NEON/SIMD state to a thread.
///
/// This function writes NEON vector registers and floating-point status/control
/// registers to an ARM64 thread using the `ARM_NEON_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to write to
/// - `regs`: The `Registers` structure containing vector and floating-point state
///
/// ## Behavior
///
/// - If no vector registers or floating-point state is set in `regs`, this function
///   returns `Ok(())` without performing any operations.
/// - Only the first 32 vector registers are written (V0-V31).
/// - FPSR and FPCR are written if they are set in `regs.floating`.
///
/// ## Returns
///
/// `Ok(())` if the NEON state was successfully written, or an error if:
/// - NEON state is not available on this system
/// - `thread_set_state()` failed
///
/// ## Errors
///
/// - `DebuggerError::InvalidArgument`: NEON state not available or `thread_set_state()` failed
///
/// ## Mach API: thread_set_state()
///
/// **Flavor**: `ARM_NEON_STATE64` = 5
/// **Count**: `ARM_NEON_STATE64_COUNT` = 68
fn write_arm64_neon_state(thread: thread_act_t, regs: &Registers) -> Result<()>
{
    if regs.vector.is_empty() && regs.floating.fpsr.is_none() && regs.floating.fpcr.is_none() {
        return Ok(());
    }

    let mut state = fetch_arm64_neon_state(thread)?
        .ok_or_else(|| DebuggerError::InvalidArgument("ARM NEON state not available on this hardware build".to_string()))?;

    for (idx, value) in regs.vector.iter().take(state.v.len()).enumerate() {
        state.v[idx] = value.as_u128();
    }
    if let Some(fpsr) = regs.floating.fpsr {
        state.fpsr = fpsr;
    }
    if let Some(fpcr) = regs.floating.fpcr {
        state.fpcr = fpcr;
    }

    let kr = unsafe {
        ffi::thread_set_state(
            thread,
            constants::ARM_NEON_STATE64,
            &state as *const _ as *const natural_t,
            constants::ARM_NEON_STATE64_COUNT,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::InvalidArgument(format!(
            "thread_set_state(ARM_NEON_STATE64) failed: {}",
            kr
        )));
    }

    Ok(())
}
