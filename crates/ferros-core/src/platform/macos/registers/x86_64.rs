//! # x86-64 Register Reading and Writing
//!
//! Functions to read and write x86-64 CPU registers from macOS processes using Mach APIs.
//!
//! ## x86-64 Register Layout
//!
//! x86-64 registers include:
//!
//! - **General-purpose**: RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, R8-R15
//! - **Instruction pointer**: RIP
//! - **Flags**: RFLAGS
//! - **Segment**: CS, FS, GS
//!
//! ## Thread State Structure
//!
//! macOS stores x86-64 thread state as a C structure with 42 `u32` values:
//!
//! - General-purpose registers (RAX-R15)
//! - Instruction pointer (RIP)
//! - Flags (RFLAGS)
//! - Segment registers (CS, FS, GS)
//!
//! ## References
//!
//! - [X86_THREAD_STATE64 structure](https://opensource.apple.com/source/xnu/xnu-4570.71.2/osfmk/mach/i386/_structs.h)

use std::mem::MaybeUninit;

use libc::{mach_msg_type_number_t, natural_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::{KERN_INVALID_ARGUMENT, KERN_SUCCESS};
use tracing::debug;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::{constants, ffi};
use crate::types::{Address, Architecture, Registers, VectorRegisterValue};

/// Read x86-64 registers from a thread
///
/// This function reads the CPU registers from an x86-64 thread using the
/// `thread_get_state()` Mach API with the `X86_THREAD_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to read registers from
///
/// ## Returns
///
/// `Ok(Registers)` containing all register values, or an error if:
/// - `thread_get_state()` failed
/// - Floating-point state read failed (non-fatal, logged as debug)
///
/// ## Registers Read
///
/// - General-purpose registers: RAX, RBX, RCX, RDX, RSI, RDI, R8-R15
/// - Stack pointer (RSP)
/// - Frame pointer (RBP)
/// - Instruction pointer (RIP)
/// - Flags register (RFLAGS)
/// - Segment registers: CS, FS, GS
/// - XMM/SIMD registers: XMM0-XMM15 (if available)
/// - MXCSR register (if available)
///
/// ## Mach API: thread_get_state()
///
/// **Flavor**: `X86_THREAD_STATE64` = 4
/// **Count**: `X86_THREAD_STATE64_COUNT` = 42
///
/// ## Errors
///
/// - `DebuggerError::ReadRegistersFailed`: `thread_get_state()` failed
///
/// ## See Also
///
/// - [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
pub fn read_registers_x86_64(thread: thread_act_t) -> Result<Registers>
{
    /// x86-64 thread state structure matching macOS `x86_thread_state64_t`.
    ///
    /// This structure represents the general-purpose register state for x86-64,
    /// including all 64-bit registers, instruction pointer, flags, and segment registers.
    #[repr(C)]
    #[derive(Default)]
    struct X86ThreadState64
    {
        /// RAX - Accumulator register
        rax: u64,
        /// RBX - Base register
        rbx: u64,
        /// RCX - Counter register
        rcx: u64,
        /// RDX - Data register
        rdx: u64,
        /// RDI - Destination index register
        rdi: u64,
        /// RSI - Source index register
        rsi: u64,
        /// RBP - Base pointer (frame pointer)
        rbp: u64,
        /// RSP - Stack pointer
        rsp: u64,
        /// R8 - General-purpose register (x86-64 extension)
        r8: u64,
        /// R9 - General-purpose register (x86-64 extension)
        r9: u64,
        /// R10 - General-purpose register (x86-64 extension)
        r10: u64,
        /// R11 - General-purpose register (x86-64 extension)
        r11: u64,
        /// R12 - General-purpose register (x86-64 extension)
        r12: u64,
        /// R13 - General-purpose register (x86-64 extension)
        r13: u64,
        /// R14 - General-purpose register (x86-64 extension)
        r14: u64,
        /// R15 - General-purpose register (x86-64 extension)
        r15: u64,
        /// RIP - Instruction pointer (program counter)
        rip: u64,
        /// RFLAGS - Flags register (condition codes, etc.)
        rflags: u64,
        /// CS - Code segment register
        cs: u64,
        /// FS - Segment register
        fs: u64,
        /// GS - Segment register
        gs: u64,
    }

    unsafe {
        let mut state = X86ThreadState64::default();
        let mut count = constants::X86_THREAD_STATE64_COUNT;
        let result = ffi::thread_get_state(
            thread,
            constants::X86_THREAD_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::ReadRegistersFailed {
                operation: "read x86-64 thread state".to_string(),
                thread_id: None,
                details: format!("thread_get_state failed: {}", result),
            });
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

/// Write x86-64 registers to a thread
///
/// This function writes CPU registers to an x86-64 thread using the
/// `thread_set_state()` Mach API with the `X86_THREAD_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to write registers to
/// - `regs`: The `Registers` structure containing the register values to write
///
/// ## Mach API: thread_set_state()
///
/// **Flavor**: `X86_THREAD_STATE64` = 4
/// **Count**: `X86_THREAD_STATE64_COUNT` = 42
///
/// ## Registers Written
///
/// - General-purpose registers: RAX, RBX, RCX, RDX, RSI, RDI, R8-R15
/// - Stack pointer (RSP)
/// - Frame pointer (RBP)
/// - Instruction pointer (RIP)
/// - Flags register (RFLAGS)
/// - Segment registers: CS, FS, GS
/// - XMM/SIMD registers (if available)
/// - MXCSR register (if available)
///
/// ## Returns
///
/// `Ok(())` if the registers were successfully written, or an error if:
/// - `thread_set_state()` failed
/// - Floating-point state is not available (if attempting to write SIMD registers)
///
/// ## Errors
///
/// - `DebuggerError::InvalidArgument`: `thread_set_state()` failed or floating-point state unavailable
///
/// ## See Also
///
/// - [thread_set_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_set_state/)
pub fn write_registers_x86_64(thread: thread_act_t, regs: &Registers) -> Result<()>
{
    /// x86-64 thread state structure matching macOS `x86_thread_state64_t`.
    ///
    /// This structure represents the general-purpose register state for x86-64,
    /// including all 64-bit registers, instruction pointer, flags, and segment registers.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct X86ThreadState64
    {
        /// RAX - Accumulator register
        rax: u64,
        /// RBX - Base register
        rbx: u64,
        /// RCX - Counter register
        rcx: u64,
        /// RDX - Data register
        rdx: u64,
        /// RDI - Destination index register
        rdi: u64,
        /// RSI - Source index register
        rsi: u64,
        /// RBP - Base pointer (frame pointer)
        rbp: u64,
        /// RSP - Stack pointer
        rsp: u64,
        /// R8 - General-purpose register (x86-64 extension)
        r8: u64,
        /// R9 - General-purpose register (x86-64 extension)
        r9: u64,
        /// R10 - General-purpose register (x86-64 extension)
        r10: u64,
        /// R11 - General-purpose register (x86-64 extension)
        r11: u64,
        /// R12 - General-purpose register (x86-64 extension)
        r12: u64,
        /// R13 - General-purpose register (x86-64 extension)
        r13: u64,
        /// R14 - General-purpose register (x86-64 extension)
        r14: u64,
        /// R15 - General-purpose register (x86-64 extension)
        r15: u64,
        /// RIP - Instruction pointer (program counter)
        rip: u64,
        /// RFLAGS - Flags register (condition codes, etc.)
        rflags: u64,
        /// CS - Code segment register
        cs: u64,
        /// FS - Segment register
        fs: u64,
        /// GS - Segment register
        gs: u64,
    }

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
            constants::X86_THREAD_STATE64,
            &state as *const _ as *const natural_t,
            constants::X86_THREAD_STATE64_COUNT,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::InvalidArgument(format!("thread_set_state failed: {}", result)));
        }
    }

    write_x86_simd_state(thread, regs)?;

    Ok(())
}

/// MMX/ST(0-7) register structure.
///
/// Represents an 80-bit MMX/ST register (10 bytes of data + 6 bytes reserved).
/// These are legacy x87 floating-point registers.
#[repr(C)]
#[derive(Clone, Copy)]
struct MmstRegister
{
    /// Register data (80 bits = 10 bytes)
    bytes: [u8; 10],
    /// Reserved padding (6 bytes)
    reserved: [u8; 6],
}

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

/// XMM register structure.
///
/// Represents a 128-bit XMM register (XMM0-XMM15) used for SSE/AVX SIMD operations.
#[repr(C)]
#[derive(Clone, Copy)]
struct XmmRegister
{
    /// Register data (128 bits = 16 bytes)
    bytes: [u8; 16],
}

impl Default for XmmRegister
{
    fn default() -> Self
    {
        Self { bytes: [0; 16] }
    }
}

/// x86-64 floating-point and SIMD state structure.
///
/// This structure represents the complete floating-point and SIMD register state
/// for x86-64, including x87 FPU registers, XMM registers, and control/status registers.
#[repr(C)]
#[derive(Clone, Copy)]
struct X86FloatState64
{
    /// Reserved field
    fpu_reserved: [i32; 2],
    /// FPU Control Word
    fpu_fcw: u16,
    /// FPU Status Word
    fpu_fsw: u16,
    /// FPU Tag Word
    fpu_ftw: u8,
    /// Reserved
    fpu_rsrv1: u8,
    /// FPU Opcode
    fpu_fop: u16,
    /// FPU Instruction Pointer
    fpu_ip: u32,
    /// FPU Code Segment
    fpu_cs: u16,
    /// Reserved
    fpu_rsrv2: u16,
    /// FPU Data Pointer
    fpu_dp: u32,
    /// FPU Data Segment
    fpu_ds: u16,
    /// Reserved
    fpu_rsrv3: u16,
    /// MXCSR register (SSE control/status)
    fpu_mxcsr: u32,
    /// MXCSR mask
    fpu_mxcsrmask: u32,
    /// MMX/ST registers (ST0-ST7, 8 × 80-bit)
    fpu_stmm: [MmstRegister; 8],
    /// XMM registers (XMM0-XMM15, 16 × 128-bit)
    fpu_xmm: [XmmRegister; 16],
    /// Reserved padding
    fpu_rsrv4: [u8; 6 * 16],
    /// Reserved field
    fpu_reserved1: i32,
}

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

/// Fetch x86-64 floating-point and SIMD state from a thread.
///
/// This function reads the floating-point and SIMD registers (XMM, MMX, x87 FPU)
/// from an x86-64 thread using the `X86_FLOAT_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to read from
///
/// ## Returns
///
/// `Ok(Some(state))` if floating-point state is available and successfully read,
/// `Ok(None)` if floating-point state is not available on this system,
/// or an error if `thread_get_state()` failed for other reasons.
///
/// ## Mach API: thread_get_state()
///
/// **Flavor**: `X86_FLOAT_STATE64` = 5
/// **Count**: `X86_FLOAT_STATE64_COUNT` = 624
///
/// ## Note
///
/// Floating-point state may not be available on all x86-64 systems. This function
/// gracefully handles the case where it's not supported by returning `Ok(None)`.
fn fetch_x86_float_state(thread: thread_act_t) -> Result<Option<X86FloatState64>>
{
    let mut state = MaybeUninit::<X86FloatState64>::zeroed();
    let mut count = constants::X86_FLOAT_STATE64_COUNT;
    let kr = unsafe {
        ffi::thread_get_state(
            thread,
            constants::X86_FLOAT_STATE64,
            state.as_mut_ptr() as *mut natural_t,
            &mut count,
        )
    };

    if kr == KERN_SUCCESS {
        Ok(Some(unsafe { state.assume_init() }))
    } else if kr == KERN_INVALID_ARGUMENT {
        debug!("x86 FLOAT state not available on this system");
        Ok(None)
    } else {
        Err(DebuggerError::ReadRegistersFailed {
            operation: "read x86-64 floating-point state".to_string(),
            thread_id: None,
            details: format!("thread_get_state(x86_FLOAT_STATE64) failed: {}", kr),
        })
    }
}

/// Write x86-64 SIMD state to a thread.
///
/// This function writes XMM registers and MXCSR register to an x86-64 thread
/// using the `X86_FLOAT_STATE64` flavor.
///
/// ## Parameters
///
/// - `thread`: The Mach thread port to write to
/// - `regs`: The `Registers` structure containing vector and floating-point state
///
/// ## Behavior
///
/// - If no vector registers or MXCSR is set in `regs`, this function returns
///   `Ok(())` without performing any operations.
/// - Only the first 16 vector registers are written (XMM0-XMM15).
/// - MXCSR is written if it is set in `regs.floating.mxcsr`.
///
/// ## Returns
///
/// `Ok(())` if the SIMD state was successfully written, or an error if:
/// - Floating-point state is not available on this system
/// - `thread_set_state()` failed
///
/// ## Errors
///
/// - `DebuggerError::InvalidArgument`: Floating-point state not available or `thread_set_state()` failed
///
/// ## Mach API: thread_set_state()
///
/// **Flavor**: `X86_FLOAT_STATE64` = 5
/// **Count**: `X86_FLOAT_STATE64_COUNT` = 624
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
            constants::X86_FLOAT_STATE64,
            &state as *const _ as *const natural_t,
            constants::X86_FLOAT_STATE64_COUNT,
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
