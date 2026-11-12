//! # Mach Exception Handling
//!
//! Mach exception handling for the macOS backend.
//!
//! This module implements the Mach exception handling loop that receives
//! exceptions from the kernel and turns them into [`StopEvent`]s.
//!
//! ## Mach Exception Ports
//!
//! On macOS, exceptions are delivered via Mach ports. The backend registers
//! an exception port with `task_set_exception_ports()` and then receives
//! exception messages via `mach_msg()`.
//!
//! ## Stops
//!
//! When an exception arrives, the loop suspends the whole task (`task_suspend`)
//! before it reports anything, because the [`Backend`](ferros_core::Backend) contract
//! says every thread is stopped when `wait` returns an event. It counts that suspend in
//! [`ExceptionSharedState::suspends`], keeps the exception message (the faulting thread
//! waits for the reply), and sends the event over a channel that `wait` receives from.
//! Resuming replies to the held message and then balances the suspends with
//! `task_resume`.
//!
//! ## References
//!
//! - [task_set_exception_ports(3) man page](https://developer.apple.com/documentation/kernel/1538049-task_set_exception_ports/)
//! - [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418827-thread_set_state/)

use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use ferros_core::backend::StopEvent;
use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::{Address, Architecture, ThreadId};
use libc::{c_int, mach_port_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::exc::__Reply__exception_raise_t;
#[cfg(target_os = "macos")]
use mach2::exception_types::{
    EXC_ARITHMETIC, EXC_BAD_ACCESS, EXC_BAD_INSTRUCTION, EXC_BREAKPOINT, EXC_SOFTWARE, exception_type_t,
};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
#[cfg(target_os = "macos")]
use mach2::message::{
    MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE, MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_RCV_LARGE, MACH_RCV_MSG,
    MACH_SEND_MSG, mach_msg, mach_msg_body_t, mach_msg_header_t, mach_msg_port_descriptor_t, mach_msg_size_t,
    mach_msg_type_number_t,
};
#[cfg(target_os = "macos")]
use mach2::ndr::NDR_record;
#[cfg(target_os = "macos")]
use mach2::ndr::NDR_record_t;
#[cfg(target_os = "macos")]
use mach2::port::MACH_PORT_NULL;
#[cfg(target_os = "macos")]
use mach2::task::task_suspend;
use tracing::{debug, error, warn};

/// State shared by the Mach exception loop and the backend, always changed under its lock.
#[derive(Debug, Default)]
pub(crate) struct ExceptionSharedState
{
    /// Number of `task_suspend` calls the backend holds on the task and has not yet
    /// balanced with `task_resume` (the launch or attach stop, exception stops, interrupts).
    pub suspends: u32,
    /// Whether the loop holds an exception message whose thread waits for a reply.
    pub pending_exception: bool,
}

impl ExceptionSharedState
{
    pub(crate) fn new() -> Self
    {
        Self::default()
    }
}

/// Lock the shared exception state, ignoring poisoning (the state stays consistent).
pub(crate) fn lock_shared(shared: &Mutex<ExceptionSharedState>) -> std::sync::MutexGuard<'_, ExceptionSharedState>
{
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Request message of `mach_exception_raise` (MIG id 2405), which the kernel sends for
/// `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES`: like `exception_raise`, but with 64-bit
/// codes. `mach2` only defines the 32-bit `exception_raise` layout. MIG packs these
/// messages to 4 bytes.
#[cfg(target_os = "macos")]
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
struct MachExceptionRaiseRequest
{
    Head: mach_msg_header_t,
    msgh_body: mach_msg_body_t,
    thread: mach_msg_port_descriptor_t,
    task: mach_msg_port_descriptor_t,
    NDR: NDR_record_t,
    exception: exception_type_t,
    codeCnt: mach_msg_type_number_t,
    code: [i64; 2],
}

/// Receive buffer: room for the request plus the trailer the kernel appends.
#[cfg(target_os = "macos")]
#[repr(C, align(8))]
struct ReceiveBuffer([u8; 1024]);

/// Command sent from the backend to the exception loop.
#[derive(Debug)]
pub(crate) enum ExceptionLoopCommand
{
    /// Reply to the held exception; the result of the reply is sent back.
    Continue(mpsc::Sender<std::result::Result<(), String>>),
    /// Reply to the held exception, if any, and exit the loop.
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

/// Convert a Mach exception to a [`StopEvent`].
///
/// Maps Mach exception types to portable stop events.
///
/// ## Exception Types
///
/// - `EXC_BREAKPOINT` -> `StopEvent::Breakpoint` with the raw `pc` (not rewound; on
///   arm64 it already points at the `brk`, and the engine maps it with
///   `Arch::pc_after_trap`)
/// - `EXC_BAD_ACCESS` -> `StopEvent::Signal(SIGSEGV)`
/// - `EXC_BAD_INSTRUCTION` -> `StopEvent::Signal(SIGILL)`
/// - `EXC_ARITHMETIC` -> `StopEvent::Signal(SIGFPE)`
/// - `EXC_SOFTWARE` -> `StopEvent::Signal(SIGTRAP)`
/// - anything else (or a breakpoint whose pc cannot be read) -> `StopEvent::Other`
///
/// See: [Mach Exception Types](https://developer.apple.com/documentation/kernel/1402149-exception_types/)
pub(crate) fn stop_event_from_exception(
    exception: exception_type_t,
    thread: ThreadId,
    pc: Option<u64>,
    codes: [i64; 2],
) -> StopEvent
{
    let signal = |signal| StopEvent::Signal { thread, signal };
    match exception as u32 {
        EXC_BREAKPOINT => match pc {
            Some(pc) => StopEvent::Breakpoint {
                thread,
                pc: Address::from(pc),
            },
            None => StopEvent::Other {
                thread: Some(thread),
                reason: format!("EXC_BREAKPOINT (pc unreadable), codes {codes:?}"),
            },
        },
        EXC_BAD_ACCESS => signal(libc::SIGSEGV),
        EXC_BAD_INSTRUCTION => signal(libc::SIGILL),
        EXC_ARITHMETIC => signal(libc::SIGFPE),
        EXC_SOFTWARE => signal(libc::SIGTRAP),
        other => StopEvent::Other {
            thread: Some(thread),
            reason: format!("Mach exception {other}, codes {codes:?}"),
        },
    }
}

/// Main exception handling loop that receives Mach exceptions.
///
/// This function runs in a separate thread and continuously receives
/// exception messages from the Mach kernel via `mach_msg()`. When an
/// exception is received, it:
///
/// 1. Suspends the whole task (`task_suspend`) and counts the suspend
/// 2. Reads the raw pc of the thread for a breakpoint
/// 3. Marks the exception as held and sends the [`StopEvent`] to `events`
/// 4. Waits for a command from the backend
/// 5. Sends an exception reply via `send_exception_reply()`
///
/// The loop exits on [`ExceptionLoopCommand::Shutdown`] (replying first, so the thread
/// can continue after a detach) or when the exception port is destroyed.
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
    task: mach_port_t,
    commands: mpsc::Receiver<ExceptionLoopCommand>,
    shared: Arc<Mutex<ExceptionSharedState>>,
    architecture: Architecture,
    events: mpsc::Sender<StopEvent>,
)
{
    loop {
        let mut buffer = MaybeUninit::<ReceiveBuffer>::zeroed();
        let recv_size = std::mem::size_of::<ReceiveBuffer>() as mach_msg_size_t;

        let kr = unsafe {
            mach_msg(
                buffer.as_mut_ptr() as *mut mach_msg_header_t,
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

        // SAFETY: mach_msg succeeded, so the buffer holds a complete message; the read is
        // unaligned-safe and the buffer is larger than the request.
        let message = unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const MachExceptionRaiseRequest) };
        let thread_port = message.thread.name as thread_act_t;
        let code = message.code;
        let codes = [code[0], code[1]];
        let exception_type = message.exception;

        {
            // Stop every thread before reporting, under the lock so a concurrent resume
            // sees either none or all of this stop.
            let mut state = lock_shared(&shared);
            let kr = unsafe { task_suspend(task) };
            if kr == KERN_SUCCESS {
                state.suspends += 1;
            } else {
                error!("task_suspend failed while handling a Mach exception: {kr}");
            }
            state.pending_exception = true;
        }

        let pc = if exception_type == EXC_BREAKPOINT as exception_type_t {
            match crate::task::read_registers_for_port(architecture, thread_port) {
                Ok(registers) => Some(registers.pc.value()),
                Err(err) => {
                    error!("Failed to read the pc of a breakpoint stop: {err}");
                    None
                }
            }
        } else {
            None
        };

        let event = stop_event_from_exception(exception_type, ThreadId::from(u64::from(thread_port)), pc, codes);
        if let Err(err) = events.send(event) {
            warn!("Failed to send stop event from Mach loop: {err}");
        }

        let command = commands.recv();
        let header = message.Head;
        let reply = send_exception_reply(&header).map_err(|err| err.to_string());
        lock_shared(&shared).pending_exception = false;
        if let Err(err) = &reply {
            error!("Failed to send Mach exception reply: {err}");
        }
        match command {
            Ok(ExceptionLoopCommand::Continue(ack)) => {
                let failed = reply.is_err();
                let _ = ack.send(reply);
                if failed {
                    break;
                }
            }
            Ok(ExceptionLoopCommand::Shutdown) | Err(_) => break,
        }
    }
}

/// Send a reply to a Mach exception message.
///
/// After processing an exception, the debugger must send a reply to the
/// kernel indicating that the exception has been handled. This allows
/// the target process to continue execution.
///
/// Uses `mach_msg()` with `MACH_SEND_MSG` to send the reply. In a received message the
/// header's `msgh_remote_port` holds the kernel's send-once reply right, so that is where
/// the reply goes; its id is the request id plus 100.
///
/// See: [mach_msg(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_msg/)
#[cfg(target_os = "macos")]
fn send_exception_reply(request: &mach_msg_header_t) -> Result<()>
{
    let mut reply = __Reply__exception_raise_t {
        Head: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
            msgh_size: std::mem::size_of::<__Reply__exception_raise_t>() as mach_msg_size_t,
            msgh_remote_port: request.msgh_remote_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: request.msgh_id + 100,
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
