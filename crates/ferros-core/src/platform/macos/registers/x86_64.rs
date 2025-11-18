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
/// ## Mach API: thread_get_state()
///
/// **Flavor**: `X86_THREAD_STATE64` = 4
/// **Count**: `X86_THREAD_STATE64_COUNT` = 42
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

#[repr(C)]
#[derive(Clone, Copy)]
struct MmstRegister
{
    bytes: [u8; 10],
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

#[repr(C)]
#[derive(Clone, Copy)]
struct XmmRegister
{
    bytes: [u8; 16],
}

impl Default for XmmRegister
{
    fn default() -> Self
    {
        Self { bytes: [0; 16] }
    }
}

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
