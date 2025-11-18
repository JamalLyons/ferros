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
#[cfg(target_arch = "x86_64")]
use std::mem::MaybeUninit;

use libc::{c_int, mach_msg_type_number_t, natural_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_INVALID_ARGUMENT;
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
use tracing::debug;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::ffi;
use crate::types::{Address, Architecture, Registers, VectorRegisterValue};

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

        if let Some(neon) = fetch_arm64_neon_state(thread)? {
            regs.vector = neon.v.iter().map(|&value| VectorRegisterValue::from_u128(value)).collect();
            regs.floating.fpsr = Some(neon.fpsr);
            regs.floating.fpcr = Some(neon.fpcr);
        }

        Ok(regs)
    }
}

#[cfg(target_arch = "aarch64")]
pub fn write_registers_arm64(thread: thread_act_t, regs: &Registers) -> Result<()>
{
    const ARM_THREAD_STATE64: c_int = 6;
    const ARM_THREAD_STATE64_COUNT: mach_msg_type_number_t = 68;

    let mut state_words: [natural_t; ARM_THREAD_STATE64_COUNT as usize] = [0; ARM_THREAD_STATE64_COUNT as usize];

    let mut write_u64 = |idx: usize, value: u64| {
        state_words[idx * 2] = (value & 0xFFFF_FFFF) as natural_t;
        state_words[idx * 2 + 1] = (value >> 32) as natural_t;
    };

    // General-purpose registers X0-X30
    for i in 0..=30 {
        let mut value = regs.general.get(i).copied().unwrap_or(0);
        if i == 29 {
            value = regs.fp.value();
        }
        write_u64(i, value);
    }

    // SP and PC
    write_u64(31, regs.sp.value());
    write_u64(32, regs.pc.value());

    // Status/CPSR (index 66, single u32)
    state_words[66] = (regs.status & 0xFFFF_FFFF) as natural_t;

    unsafe {
        let result = ffi::thread_set_state(thread, ARM_THREAD_STATE64, state_words.as_ptr(), ARM_THREAD_STATE64_COUNT);

        if result != KERN_SUCCESS {
            return Err(DebuggerError::InvalidArgument(format!("thread_set_state failed: {}", result)));
        }
    }

    write_arm64_neon_state(thread, regs)?;

    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn _unused_compile_assert_arm64_write() {}

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

        if let Some(float_state) = fetch_x86_float_state(thread)? {
            regs.vector = float_state
                .fpu_xmm
                .iter()
                .map(|reg| VectorRegisterValue::from_bytes(reg.bytes))
                .collect();
            regs.floating.mxcsr = Some(float_state.fpu_mxcsr);
        }

        Ok(regs)
    }
}

#[cfg(target_arch = "x86_64")]
pub fn write_registers_x86_64(thread: thread_act_t, regs: &Registers) -> Result<()>
{
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
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

    let mut state = X86ThreadState64::default();
    let general = |idx| regs.general.get(idx).copied().unwrap_or(0);

    state.rax = general(0);
    state.rbx = general(1);
    state.rcx = general(2);
    state.rdx = general(3);
    state.rsi = general(4);
    state.rdi = general(5);
    state.r8 = general(6);
    state.r9 = general(7);
    state.r10 = general(8);
    state.r11 = general(9);
    state.r12 = general(10);
    state.r13 = general(11);
    state.r14 = general(12);
    state.r15 = general(13);
    state.rbp = regs.fp.value();
    state.rsp = regs.sp.value();
    state.rip = regs.pc.value();
    state.rflags = regs.status;

    unsafe {
        let result = ffi::thread_set_state(
            thread,
            X86_THREAD_STATE64,
            &state as *const _ as *const natural_t,
            X86_THREAD_STATE64_COUNT,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::InvalidArgument(format!("thread_set_state failed: {}", result)));
        }
    }

    write_x86_simd_state(thread, regs)?;

    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn _unused_compile_assert_x86_write() {}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct ArmNeonState64
{
    v: [u128; 32],
    fpsr: u32,
    fpcr: u32,
}

#[cfg(target_arch = "aarch64")]
impl Default for ArmNeonState64
{
    fn default() -> Self
    {
        Self {
            v: [0; 32],
            fpsr: 0,
            fpcr: 0,
        }
    }
}

#[cfg(target_arch = "aarch64")]
const ARM_NEON_STATE64: c_int = 17;
#[cfg(target_arch = "aarch64")]
const ARM_NEON_STATE64_COUNT: mach_msg_type_number_t =
    (std::mem::size_of::<ArmNeonState64>() / std::mem::size_of::<natural_t>()) as mach_msg_type_number_t;

#[cfg(target_arch = "aarch64")]
fn fetch_arm64_neon_state(thread: thread_act_t) -> Result<Option<ArmNeonState64>>
{
    let mut state = ArmNeonState64::default();
    let mut count = ARM_NEON_STATE64_COUNT;
    let kr = unsafe { ffi::thread_get_state(thread, ARM_NEON_STATE64, &mut state as *mut _ as *mut natural_t, &mut count) };

    if kr == KERN_SUCCESS {
        Ok(Some(state))
    } else if kr == KERN_INVALID_ARGUMENT {
        debug!("ARM NEON state not available on this system");
        Ok(None)
    } else {
        Err(DebuggerError::ReadRegistersFailed(format!(
            "thread_get_state(ARM_NEON_STATE64) failed: {}",
            kr
        )))
    }
}

#[cfg(target_arch = "aarch64")]
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
            ARM_NEON_STATE64,
            &state as *const _ as *const natural_t,
            ARM_NEON_STATE64_COUNT,
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

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MmstRegister
{
    bytes: [u8; 10],
    reserved: [u8; 6],
}

#[cfg(target_arch = "x86_64")]
impl Default for MmstRegister
{
    fn default() -> Self
    {
        Self {
            bytes: [0; 10],
            reserved: [0; 6],
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct XmmRegister
{
    bytes: [u8; 16],
}

#[cfg(target_arch = "x86_64")]
impl Default for XmmRegister
{
    fn default() -> Self
    {
        Self { bytes: [0; 16] }
    }
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct X86FloatState64
{
    fpu_reserved: [i32; 2],
    fpu_fcw: u16,
    fpu_fsw: u16,
    fpu_ftw: u8,
    fpu_rsrv1: u8,
    fpu_fop: u16,
    fpu_ip: u32,
    fpu_cs: u16,
    fpu_rsrv2: u16,
    fpu_dp: u32,
    fpu_ds: u16,
    fpu_rsrv3: u16,
    fpu_mxcsr: u32,
    fpu_mxcsrmask: u32,
    fpu_stmm: [MmstRegister; 8],
    fpu_xmm: [XmmRegister; 16],
    fpu_rsrv4: [u8; 6 * 16],
    fpu_reserved1: i32,
}

#[cfg(target_arch = "x86_64")]
impl Default for X86FloatState64
{
    fn default() -> Self
    {
        Self {
            fpu_reserved: [0; 2],
            fpu_fcw: 0,
            fpu_fsw: 0,
            fpu_ftw: 0,
            fpu_rsrv1: 0,
            fpu_fop: 0,
            fpu_ip: 0,
            fpu_cs: 0,
            fpu_rsrv2: 0,
            fpu_dp: 0,
            fpu_ds: 0,
            fpu_rsrv3: 0,
            fpu_mxcsr: 0,
            fpu_mxcsrmask: 0,
            fpu_stmm: [MmstRegister::default(); 8],
            fpu_xmm: [XmmRegister::default(); 16],
            fpu_rsrv4: [0; 6 * 16],
            fpu_reserved1: 0,
        }
    }
}

#[cfg(target_arch = "x86_64")]
const X86_FLOAT_STATE64: c_int = 5;
#[cfg(target_arch = "x86_64")]
const X86_FLOAT_STATE64_COUNT: mach_msg_type_number_t =
    (std::mem::size_of::<X86FloatState64>() / std::mem::size_of::<natural_t>()) as mach_msg_type_number_t;

#[cfg(target_arch = "x86_64")]
fn fetch_x86_float_state(thread: thread_act_t) -> Result<Option<X86FloatState64>>
{
    let mut state = MaybeUninit::<X86FloatState64>::zeroed();
    let mut count = X86_FLOAT_STATE64_COUNT;
    let kr = unsafe { ffi::thread_get_state(thread, X86_FLOAT_STATE64, state.as_mut_ptr() as *mut natural_t, &mut count) };

    if kr == KERN_SUCCESS {
        Ok(Some(unsafe { state.assume_init() }))
    } else if kr == KERN_INVALID_ARGUMENT {
        debug!("x86 FLOAT state not available on this system");
        Ok(None)
    } else {
        Err(DebuggerError::ReadRegistersFailed(format!(
            "thread_get_state(x86_FLOAT_STATE64) failed: {}",
            kr
        )))
    }
}

#[cfg(target_arch = "x86_64")]
fn write_x86_simd_state(thread: thread_act_t, regs: &Registers) -> Result<()>
{
    if regs.vector.is_empty() && regs.floating.mxcsr.is_none() {
        return Ok(());
    }

    let mut state = fetch_x86_float_state(thread)?.ok_or_else(|| {
        DebuggerError::InvalidArgument("x86 floating-point state not available on this hardware build".to_string())
    })?;

    for (idx, value) in regs.vector.iter().take(state.fpu_xmm.len()).enumerate() {
        state.fpu_xmm[idx].bytes.copy_from_slice(value.bytes());
    }

    if let Some(mxcsr) = regs.floating.mxcsr {
        state.fpu_mxcsr = mxcsr;
    }

    let kr = unsafe {
        ffi::thread_set_state(
            thread,
            X86_FLOAT_STATE64,
            &state as *const _ as *const natural_t,
            X86_FLOAT_STATE64_COUNT,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::InvalidArgument(format!(
            "thread_set_state(x86_FLOAT_STATE64) failed: {}",
            kr
        )));
    }

    Ok(())
}
