//! # Mach Exception Handling
//!
//! Mach exception handling for macOS debugger.
//!
//! This module implements the Mach exception handling loop that receives
//! exceptions from the kernel and processes them (breakpoints, signals, etc.).
//!
//! ## Mach Exception Ports
//!
//! On macOS, exceptions are delivered via Mach ports. The debugger registers
//! an exception port with `task_set_exception_ports()` and then receives
//! exception messages via `mach_msg()`.
//!
//! ## References
//!
//! - [task_set_exception_ports(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_set_exception_ports/)
//! - [mach_msg(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_msg/)
//! - [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
//! - [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_set_state/)

use std::mem::MaybeUninit;
use std::sync::{mpsc, Arc, Mutex};

use libc::{c_int, mach_port_t, natural_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::exc::{__Reply__exception_raise_t, __Request__exception_raise_t};
#[cfg(target_os = "macos")]
use mach2::exception_types::{
    exception_type_t, EXC_ARITHMETIC, EXC_BAD_ACCESS, EXC_BAD_INSTRUCTION, EXC_BREAKPOINT, EXC_SOFTWARE,
};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::message::{
    mach_msg, mach_msg_header_t, mach_msg_size_t, MACH_MSGH_BITS, MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE,
    MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_RCV_LARGE, MACH_RCV_MSG, MACH_SEND_MSG,
};
#[cfg(target_os = "macos")]
use mach2::ndr::NDR_record;
#[cfg(target_os = "macos")]
use mach2::port::MACH_PORT_NULL;
use tracing::{debug, error, warn};

use crate::breakpoints::BreakpointStore;
use crate::error::{DebuggerError, Result};
use crate::events::{self, DebuggerEvent};
use crate::platform::macos::{constants, ffi};
use crate::types::{Address, Architecture, StopReason, ThreadId};

/// Shared exception state manipulated by the Mach exception loop and debugger methods.
#[derive(Debug)]
pub(crate) struct ExceptionSharedState
{
    pub stopped: bool,
    pub stop_reason: StopReason,
    pub pending_thread: Option<thread_act_t>,
}

impl ExceptionSharedState
{
    pub(crate) fn new() -> Self
    {
        Self {
            stopped: false,
            stop_reason: StopReason::Running,
            pending_thread: None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ExceptionLoopCommand
{
    Continue,
    Shutdown,
}

/// Get the thread state flavor for the given architecture.
///
/// Returns the appropriate thread state flavor constant for use with
/// `thread_get_state()` and `thread_set_state()`.
///
/// ## Thread State Flavors
///
/// - **ARM64**: Returns `6` (`ARM_THREAD_STATE64`)
/// - **x86-64**: Returns `4` (`x86_THREAD_STATE64`)
///
/// See: [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
pub(crate) fn thread_state_flavor_for_arch(architecture: Architecture) -> c_int
{
    match architecture {
        Architecture::Arm64 => 6,
        Architecture::X86_64 => 4,
        Architecture::Unknown(_) => 6,
    }
}

/// Rewind the program counter after a breakpoint hit.
///
/// When a breakpoint is hit, the PC points to the instruction after the
/// breakpoint. This function rewinds it to point at the breakpoint instruction
/// itself, allowing the debugger to single-step over it.
///
/// ## Architecture-Specific Behavior
///
/// - **ARM64**: Subtracts 4 bytes (instruction size)
/// - **x86-64**: Subtracts 1 byte (INT3 instruction size)
///
/// Uses `thread_get_state()` and `thread_set_state()` to read and modify
/// the thread's register state.
///
/// See:
/// - [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
/// - [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_set_state/)
pub(crate) fn rewind_breakpoint_pc(thread: thread_act_t, architecture: Architecture) -> Result<Option<u64>>
{
    match architecture {
        Architecture::Arm64 => rewind_breakpoint_pc_arm64(thread),
        Architecture::X86_64 => rewind_breakpoint_pc_x86(thread),
        Architecture::Unknown(_) => Ok(None),
    }
}

#[cfg(target_arch = "aarch64")]
fn rewind_breakpoint_pc_arm64(thread: thread_act_t) -> Result<Option<u64>>
{
    unsafe {
        let mut state: [natural_t; constants::ARM_THREAD_STATE64_COUNT as usize] =
            [0; constants::ARM_THREAD_STATE64_COUNT as usize];
        let mut count = constants::ARM_THREAD_STATE64_COUNT;
        let mut kr = ffi::thread_get_state(thread, constants::ARM_THREAD_STATE64, state.as_mut_ptr(), &mut count);
        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        let read_u64 = |idx: usize, buf: &[natural_t]| -> u64 {
            let lo = buf[idx * 2] as u64;
            let hi = buf[idx * 2 + 1] as u64;
            lo | (hi << 32)
        };

        let pc = read_u64(constants::ARM64_PC_INDEX, &state);
        let new_pc = pc.saturating_sub(constants::ARM64_INSTRUCTION_SIZE);
        state[constants::ARM64_PC_INDEX_LOW] = (new_pc & constants::U32_MASK) as natural_t;
        state[constants::ARM64_PC_INDEX_HIGH] = (new_pc >> 32) as natural_t;

        kr = ffi::thread_set_state(
            thread,
            constants::ARM_THREAD_STATE64,
            state.as_ptr(),
            constants::ARM_THREAD_STATE64_COUNT,
        );
        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(Some(new_pc))
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn rewind_breakpoint_pc_arm64(_thread: thread_act_t) -> Result<Option<u64>>
{
    Ok(None)
}

#[cfg(target_arch = "x86_64")]
fn rewind_breakpoint_pc_x86(thread: thread_act_t) -> Result<Option<u64>>
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

    // Use constants from the centralized constants module

    unsafe {
        let mut state = X86ThreadState64::default();
        let mut count = constants::X86_THREAD_STATE64_COUNT;
        let mut kr = ffi::thread_get_state(
            thread,
            constants::X86_THREAD_STATE64,
            &mut state as *mut _ as *mut natural_t,
            &mut count,
        );

        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        let new_pc = state.rip.saturating_sub(constants::X86_64_INSTRUCTION_SIZE);
        state.rip = new_pc;

        kr = ffi::thread_set_state(
            thread,
            constants::X86_THREAD_STATE64,
            &state as *const _ as *const natural_t,
            constants::X86_THREAD_STATE64_COUNT,
        );
        if kr != KERN_SUCCESS {
            return Err(DebuggerError::MachError(kr.into()));
        }

        Ok(Some(new_pc))
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn rewind_breakpoint_pc_x86(_thread: thread_act_t) -> Result<Option<u64>>
{
    Ok(None)
}

/// Convert a Mach exception to a StopReason.
///
/// Maps Mach exception types to platform-agnostic stop reasons.
///
/// ## Exception Types
///
/// - `EXC_BREAKPOINT` → `StopReason::Breakpoint`
/// - `EXC_BAD_ACCESS` → `StopReason::Signal(SIGSEGV)`
/// - `EXC_BAD_INSTRUCTION` → `StopReason::Signal(SIGILL)`
/// - `EXC_ARITHMETIC` → `StopReason::Signal(SIGFPE)`
/// - `EXC_SOFTWARE` → `StopReason::Signal(SIGTRAP)`
///
/// See: [Mach Exception Types](https://developer.apple.com/documentation/kernel/1402149-exception_types/)
pub(crate) fn stop_reason_from_exception(exception: exception_type_t, pc: Option<u64>, _codes: [i64; 2]) -> StopReason
{
    match exception as u32 {
        EXC_BREAKPOINT => StopReason::Breakpoint(pc.unwrap_or(0)),
        EXC_BAD_ACCESS => StopReason::Signal(libc::SIGSEGV),
        EXC_BAD_INSTRUCTION => StopReason::Signal(libc::SIGILL),
        EXC_ARITHMETIC => StopReason::Signal(libc::SIGFPE),
        EXC_SOFTWARE => StopReason::Signal(libc::SIGTRAP),
        _ => StopReason::Unknown,
    }
}

/// Main exception handling loop that receives Mach exceptions.
///
/// This function runs in a separate thread and continuously receives
/// exception messages from the Mach kernel via `mach_msg()`. When an
/// exception is received, it:
///
/// 1. Determines the stop reason (breakpoint, signal, etc.)
/// 2. Updates shared exception state
/// 3. Sends a `DebuggerEvent::TargetStopped` event
/// 4. Waits for a resume command
/// 5. Sends an exception reply via `send_exception_reply()`
///
/// ## Mach Message Protocol
///
/// The loop uses `mach_msg()` with `MACH_RCV_MSG` to receive exception
/// messages. Each message contains:
/// - Exception type (breakpoint, bad access, etc.)
/// - Thread port
/// - Exception codes
///
/// See:
/// - [mach_msg(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_msg/)
/// - [Mach Exception Handling](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
#[cfg(target_os = "macos")]
pub(crate) fn run_exception_loop(
    exception_port: mach_port_t,
    resume_rx: mpsc::Receiver<ExceptionLoopCommand>,
    shared_state: Arc<Mutex<ExceptionSharedState>>,
    architecture: Architecture,
    event_tx: events::DebuggerEventSender,
    breakpoints: Arc<Mutex<BreakpointStore>>,
)
{
    loop {
        let mut request = MaybeUninit::<__Request__exception_raise_t>::uninit();
        let recv_size = std::mem::size_of::<__Request__exception_raise_t>() as mach_msg_size_t;

        let kr = unsafe {
            mach_msg(
                request.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_RCV_MSG | MACH_RCV_LARGE,
                0,
                recv_size,
                exception_port,
                MACH_MSG_TIMEOUT_NONE,
                MACH_PORT_NULL,
            )
        };

        if kr != MACH_MSG_SUCCESS {
            if kr == mach2::message::MACH_RCV_PORT_DIED || kr == mach2::message::MACH_RCV_INVALID_NAME {
                debug!("Mach exception port closed, exiting handler loop");
                break;
            }
            continue;
        }

        let message = unsafe { request.assume_init() };
        let thread_port = message.thread.name as thread_act_t;
        let codes = [message.code[0] as i64, message.code[1] as i64];

        let rewound_pc = if message.exception == EXC_BREAKPOINT as exception_type_t {
            match rewind_breakpoint_pc(thread_port, architecture) {
                Ok(value) => value,
                Err(err) => {
                    error!("Failed to rewind breakpoint PC: {err}");
                    None
                }
            }
        } else {
            None
        };

        let stop_reason = stop_reason_from_exception(message.exception, rewound_pc, codes);
        {
            let mut shared = shared_state.lock().unwrap();
            shared.stopped = true;
            shared.stop_reason = stop_reason;
            shared.pending_thread = Some(thread_port);
        }

        if let StopReason::Breakpoint(addr) = stop_reason {
            let mut store = breakpoints.lock().unwrap();
            store.record_hit(Address::from(addr));
        }

        if let Err(err) = event_tx.send(DebuggerEvent::TargetStopped {
            reason: stop_reason,
            thread: Some(ThreadId::from(thread_port as u64)),
        }) {
            warn!("Failed to send stop event from Mach loop: {err}");
        }

        match resume_rx.recv() {
            Ok(ExceptionLoopCommand::Continue) => {
                if let Err(err) = send_exception_reply(&message) {
                    error!("Failed to send Mach exception reply: {err}");
                    break;
                }

                let mut shared = shared_state.lock().unwrap();
                shared.stopped = false;
                shared.stop_reason = StopReason::Running;
                shared.pending_thread = None;

                if let Err(err) = event_tx.send(DebuggerEvent::TargetResumed) {
                    warn!("Failed to send resume event from Mach loop: {err}");
                }
            }
            Ok(ExceptionLoopCommand::Shutdown) | Err(_) => {
                let mut shared = shared_state.lock().unwrap();
                shared.stopped = false;
                shared.stop_reason = StopReason::Running;
                shared.pending_thread = None;
                break;
            }
        }
    }
}

/// Send a reply to a Mach exception message.
///
/// After processing an exception, the debugger must send a reply to the
/// kernel indicating that the exception has been handled. This allows
/// the target process to continue execution.
///
/// Uses `mach_msg()` with `MACH_SEND_MSG` to send the reply.
///
/// See: [mach_msg(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_msg/)
#[cfg(target_os = "macos")]
fn send_exception_reply(request: &__Request__exception_raise_t) -> Result<()>
{
    let mut reply = __Reply__exception_raise_t {
        Head: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
            msgh_size: std::mem::size_of::<__Reply__exception_raise_t>() as mach_msg_size_t,
            msgh_remote_port: request.Head.msgh_local_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: request.Head.msgh_id + 100,
        },
        NDR: unsafe { NDR_record },
        RetCode: KERN_SUCCESS,
    };

    let kr = unsafe {
        mach_msg(
            &mut reply.Head,
            MACH_SEND_MSG,
            reply.Head.msgh_size,
            0,
            MACH_PORT_NULL,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };

    if kr != MACH_MSG_SUCCESS {
        return Err(DebuggerError::ResumeFailed(format!("mach_msg reply failed: {}", kr)));
    }

    Ok(())
}
