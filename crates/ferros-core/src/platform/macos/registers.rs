//! # macOS Register Reading
//!
//! Functions to read CPU registers from macOS processes using Mach APIs.
//!
//! On macOS, registers are read using `thread_get_state()` with architecture-specific
//! "flavors" that specify which registers to read:
//!
//! - **ARM64**: `ARM_THREAD_STATE64` (flavor 6)
//! - **x86-64**: `X86_THREAD_STATE64` (flavor 4)
//!
//! ## Register Layout
//!
//! macOS stores registers as arrays of `natural_t` (which is `u32`). Each 64-bit
//! register is stored as two `u32` values in little-endian format:
///
/// - Low 32 bits at index `i * 2`
/// - High 32 bits at index `i * 2 + 1`
///
/// ## References
///
/// - [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
/// - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
/// - [ARM_THREAD_STATE64 structure](https://opensource.apple.com/source/xnu/xnu-4570.71.2/osfmk/mach/arm/_structs.h)
use libc::{c_int, mach_msg_type_number_t, natural_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::ffi;
use crate::types::{Address, Architecture, Registers};

/// Read ARM64 registers from a thread
///
/// This function reads the CPU registers from an ARM64 thread using the
/// `thread_get_state()` Mach API with the `ARM_THREAD_STATE64` flavor.
///
/// ## ARM64 Register Layout
///
/// ARM64 has 31 general-purpose registers (X0-X30) plus special registers:
///
/// - **X0-X28**: General-purpose registers (29 registers)
/// - **X29 (FP)**: Frame pointer
/// - **X30 (LR)**: Link register (return address)
/// - **SP**: Stack pointer
/// - **PC**: Program counter
/// - **CPSR**: Current Program Status Register (flags)
///
/// ## Thread State Structure
///
/// macOS stores ARM64 thread state as an array of 68 `u32` values:
///
/// ```rs
/// Index 0-28:   X0-X28 (29 registers × 2 u32s = 58 u32s)
/// Index 29:     FP (X29) (2 u32s)
/// Index 30:     LR (X30) (2 u32s)
/// Index 31:     SP (2 u32s)
/// Index 32:     PC (2 u32s)
/// Index 33:     CPSR + padding (2 u32s)
/// Total: 34 u64s = 68 u32s
/// ```
///
/// ## Mach API: thread_get_state()
///
/// ```c
/// kern_return_t thread_get_state(
///     thread_act_t target_act,        // Thread port from task_threads()
///     thread_state_flavor_t flavor,   // ARM_THREAD_STATE64 = 6
///     thread_state_t old_state,       // Output: array of natural_t
///     mach_msg_type_number_t *count   // Input/output: size of array
/// );
/// ```
///
/// **Flavor**: `ARM_THREAD_STATE64` = 6
/// **Count**: `ARM_THREAD_STATE64_COUNT` = 68 (number of `u32` values)
///
/// See: [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
///
/// ## References
///
/// - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
/// - [ARM CPSR Register](https://developer.arm.com/documentation/dui0801/a/A32-and-T32-Instructions/CPSR)
/// - [ARM_THREAD_STATE64 structure](https://opensource.apple.com/source/xnu/xnu-4570.71.2/osfmk/mach/arm/_structs.h)
#[cfg(target_arch = "aarch64")]
pub fn read_registers_arm64(thread: thread_act_t) -> Result<Registers>
{
    // Use mach2 crate for KERN_SUCCESS constant - better maintained than libc's version
    // Note: thread_get_state() is NOT available in mach2 (likely because it's a restricted API),
    // so we declare it ourselves using extern "C"

    // ARM64 thread state constants
    // These are defined by macOS's Mach kernel
    //
    // ARM_THREAD_STATE64 = 6: This is the "flavor" that tells macOS we want ARM64 registers
    // ARM_THREAD_STATE64_COUNT = 68: Number of u32 values in the state array
    //
    // See: /usr/include/mach/arm/_structs.h (on macOS)
    const ARM_THREAD_STATE64: c_int = 6;
    const ARM_THREAD_STATE64_COUNT: mach_msg_type_number_t = 68;

    unsafe {
        // Allocate array to hold thread state
        // macOS returns registers as an array of natural_t (u32)
        // We need 68 u32s to hold all ARM64 registers
        let mut state_words: [natural_t; ARM_THREAD_STATE64_COUNT as usize] = [0; ARM_THREAD_STATE64_COUNT as usize];
        let mut count: mach_msg_type_number_t = ARM_THREAD_STATE64_COUNT;

        // Call thread_get_state to read registers
        // This fills state_words with the current register values
        //
        let result = ffi::thread_get_state(thread, ARM_THREAD_STATE64, state_words.as_mut_ptr(), &mut count);

        // Check if the call succeeded
        // Use mach2's KERN_SUCCESS constant (better maintained than libc's version)
        if result != KERN_SUCCESS {
            return Err(DebuggerError::ReadRegistersFailed(format!(
                "thread_get_state failed: {}",
                result
            )));
        }

        // Helper function to read a u64 from two u32s
        // macOS stores 64-bit registers as two 32-bit values in little-endian format:
        // - Low 32 bits at index i * 2
        // - High 32 bits at index i * 2 + 1
        //
        // Example: To read X0 (index 0):
        // - low = state_words[0]
        // - high = state_words[1]
        // - x0 = low | (high << 32)
        let read_u64 = |idx: usize| -> u64 {
            let low = state_words[idx * 2];
            let high = state_words[idx * 2 + 1];
            (low as u64) | ((high as u64) << 32)
        };

        // Parse the register values from the state array
        // The layout is:
        // - Indices 0-28: X0-X28 (29 general-purpose registers)
        // - Index 29: FP (X29, frame pointer)
        // - Index 30: LR (X30, link register/return address)
        // - Index 31: SP (stack pointer)
        // - Index 32: PC (program counter)
        // - Index 33: CPSR (status register) - but only first u32 is used, second is padding
        let mut regs = Registers::new();
        regs.set_architecture(Architecture::Arm64);

        // Program Counter: Points to the next instruction to execute
        // This is at index 32 in the state array
        regs.pc = Address::from(read_u64(32));

        // Stack Pointer: Points to the top of the stack
        // This is at index 31
        regs.sp = Address::from(read_u64(31));

        // Frame Pointer: Points to the current stack frame
        // This is X29, at index 29
        regs.fp = Address::from(read_u64(29));

        // CPSR (Current Program Status Register): Contains flags
        // This is a u32 at index 66 (33 * 2), not a u64
        // The second u32 at index 67 is padding
        regs.status = state_words[66] as u64;

        // General-purpose registers: X0-X30
        regs.general = (0..=30).map(read_u64).collect();

        Ok(regs)
    }
}

/// Read x86-64 registers from a thread
///
/// **Not yet implemented** - will use `X86_THREAD_STATE64` flavor when ready.
///
/// ## Future Implementation
///
/// Will use:
/// - Flavor: `X86_THREAD_STATE64` = 4
/// - Count: `X86_THREAD_STATE64_COUNT` = 42
///
/// x86-64 registers include: RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, R8-R15, RIP, RFLAGS, etc.
#[cfg(target_arch = "x86_64")]
pub fn read_registers_x86_64(thread: thread_act_t) -> Result<Registers>
{
    #[repr(C)]
    #[derive(Default)]
    struct X86ThreadState64
    {
        rax: u64,
        rbx: u64,
        rcx: u64,
        rdx: u64,
        rdi: u64,
        rsi: u64,
        rbp: u64,
        rsp: u64,
        r8: u64,
        r9: u64,
        r10: u64,
        r11: u64,
        r12: u64,
        r13: u64,
        r14: u64,
        r15: u64,
        rip: u64,
        rflags: u64,
        cs: u64,
        fs: u64,
        gs: u64,
    }

    const X86_THREAD_STATE64: c_int = 4;
    const X86_THREAD_STATE64_COUNT: mach_msg_type_number_t = 42;

    unsafe {
        let mut state = X86ThreadState64::default();
        let mut count = X86_THREAD_STATE64_COUNT;
        let result = ffi::thread_get_state(thread, X86_THREAD_STATE64, &mut state as *mut _ as *mut natural_t, &mut count);

        if result != KERN_SUCCESS {
            return Err(DebuggerError::ReadRegistersFailed(format!(
                "thread_get_state failed: {}",
                result
            )));
        }

        let mut regs = Registers::new();
        regs.set_architecture(Architecture::X86_64);
        regs.pc = Address::from(state.rip);
        regs.sp = Address::from(state.rsp);
        regs.fp = Address::from(state.rbp);
        regs.status = state.rflags;
        regs.general = vec![
            state.rax, state.rbx, state.rcx, state.rdx, state.rsi, state.rdi, state.r8, state.r9, state.r10, state.r11,
            state.r12, state.r13, state.r14, state.r15,
        ];

        Ok(regs)
    }
}
