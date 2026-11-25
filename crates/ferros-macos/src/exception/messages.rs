//! Mach messages on the exception port, saved exception ports, and replies.

#[allow(clippy::wildcard_imports)]
use super::*;

/// MIG id of `mach_exception_raise`, the request the kernel sends for
/// `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES`.
#[cfg(target_os = "macos")]
pub(super) const MACH_EXCEPTION_RAISE_ID: i32 = 2405;

/// Id of the empty message [`wake_loop`] sends to the exception port. It is outside the
/// MIG `mach_exc` subsystem (2405 to 2407), so it is never taken for a request.
#[cfg(target_os = "macos")]
pub(super) const WAKE_LOOP_ID: i32 = 0x6665_7272;

/// Wake the exception loop out of its receive on `exception_port`, so it takes a command
/// just sent to it at once.
///
/// While a stop is pending the loop alternates between a bounded receive and a check of
/// its commands. Without a wake, every `Continue` (and so every resume and instruction
/// step) waited for that receive to time out (issue #160). The send never blocks: a full
/// queue already wakes the loop. A failure only loses the wake, and the loop then takes
/// the command when its receive times out.
#[cfg(target_os = "macos")]
pub(crate) fn wake_loop(exception_port: mach_port_t)
{
    if exception_port == MACH_PORT_NULL {
        return;
    }
    let mut wake = mach_msg_header_t {
        msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0),
        msgh_size: std::mem::size_of::<mach_msg_header_t>() as mach_msg_size_t,
        msgh_remote_port: exception_port,
        msgh_local_port: MACH_PORT_NULL,
        msgh_voucher_port: MACH_PORT_NULL,
        msgh_id: WAKE_LOOP_ID,
    };
    // SAFETY: `wake` is a complete header-only message that outlives the call; the
    // backend holds a send right to `exception_port` until it destroys the port, and
    // `MACH_MSG_TYPE_COPY_SEND` leaves that right with it.
    let kr = unsafe {
        mach_msg(
            &mut wake,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            wake.msgh_size,
            0,
            MACH_PORT_NULL,
            0,
            MACH_PORT_NULL,
        )
    };
    if kr != MACH_MSG_SUCCESS {
        debug!("could not wake the exception loop: mach_msg returned {kr:#x}");
    }
}

/// Request message of `mach_exception_raise` (MIG id 2405), which the kernel sends for
/// `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES`: like `exception_raise`, but with 64-bit
/// codes. `mach2` only defines the 32-bit `exception_raise` layout. MIG packs these
/// messages to 4 bytes.
#[cfg(target_os = "macos")]
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
pub(super) struct MachExceptionRaiseRequest
{
    pub(super) Head: mach_msg_header_t,
    pub(super) msgh_body: mach_msg_body_t,
    pub(super) thread: mach_msg_port_descriptor_t,
    pub(super) task: mach_msg_port_descriptor_t,
    pub(super) NDR: NDR_record_t,
    pub(super) exception: exception_type_t,
    pub(super) codeCnt: mach_msg_type_number_t,
    pub(super) code: [i64; 2],
}

/// Receive buffer: room for the request plus the trailer the kernel appends.
#[cfg(target_os = "macos")]
#[repr(C, align(8))]
pub(super) struct ReceiveBuffer(pub(super) [u8; 1024]);

/// A logical exception stop, retaining its thread right and any unanswered reply.
#[derive(Debug)]
pub(crate) struct HeldException
{
    /// An unanswered request's send-once reply right; None after it is consumed.
    pub(super) header: Option<mach_msg_header_t>,
    /// The message's thread send right, retained until its logical stop is released.
    pub(super) thread: mach_port_t,
    pub(super) soft_signal: bool,
    /// [`ExceptionSharedState::handshake_generation`] when the loop received it.
    pub(super) handshake: u64,
}

/// A requeued signal owns a thread right even if it remains masked while
/// the backend refreshes its thread list. Its port name cannot be reused meanwhile.
#[derive(Debug)]
pub(crate) struct RequeuedSignal
{
    pub(crate) thread: thread_act_t,
    pub(crate) signal: c_int,
}

#[cfg(target_os = "macos")]
impl RequeuedSignal
{
    pub(crate) fn new(thread: thread_act_t, signal: c_int) -> BackendResult<Self>
    {
        // SAFETY: the held exception owns a send right to this thread; retain an
        // independent reference until delivery or shutdown.
        let kr =
            unsafe { mach2::mach_port::mach_port_mod_refs(mach_task_self(), thread, mach2::port::MACH_PORT_RIGHT_SEND, 1) };
        if kr != KERN_SUCCESS {
            return Err(BackendError::Os {
                code: i64::from(kr),
                message: "failed to retain requeued signal thread".into(),
            });
        }
        Ok(Self { thread, signal })
    }
}

#[cfg(target_os = "macos")]
impl Drop for RequeuedSignal
{
    fn drop(&mut self)
    {
        deallocate_right(self.thread);
    }
}

/// Command sent from the backend to the exception loop.
#[derive(Debug)]
pub(crate) enum ExceptionLoopCommand
{
    /// Release stop records except for threads in `keep`, including any outstanding
    /// signal replies (see [`answer_held_exceptions`]). Acknowledge after cleanup.
    Continue
    {
        /// Receives `()` once the records were released.
        ack: mpsc::Sender<()>,
        /// Threads whose stop records and deferred signal decisions must remain.
        keep: Vec<thread_act_t>,
        /// Teardown discards stops deferred before the handshake began.
        discard_deferred: bool,
    },
    /// Reply to every held exception (kept ones included), answer the queued ones, and
    /// exit the loop.
    Shutdown,
}

/// Capacity of the arrays passed to `task_get_exception_ports` /
/// `task_swap_exception_ports` (the kernel has fewer than this many exception types).
#[cfg(target_os = "macos")]
pub(super) const EXCEPTION_PORT_SLOTS: usize = 32;

/// One group of a task's exception ports: the exception types in `mask` share
/// `handler`, `behavior` and `flavor`. `handler` is a send right owned by whoever
/// obtained the entry (or `MACH_PORT_NULL` / `MACH_PORT_DEAD`).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExceptionPortEntry
{
    pub mask: exception_mask_t,
    pub handler: mach_port_t,
    pub behavior: exception_behavior_t,
    pub flavor: thread_state_flavor_t,
}

/// Arrays filled by `task_get_exception_ports` and `task_swap_exception_ports`.
#[cfg(target_os = "macos")]
pub(super) struct ExceptionPortArrays
{
    pub(super) masks: [exception_mask_t; EXCEPTION_PORT_SLOTS],
    pub(super) handlers: [mach_port_t; EXCEPTION_PORT_SLOTS],
    pub(super) behaviors: [exception_behavior_t; EXCEPTION_PORT_SLOTS],
    pub(super) flavors: [thread_state_flavor_t; EXCEPTION_PORT_SLOTS],
    pub(super) count: mach_msg_type_number_t,
}

#[cfg(target_os = "macos")]
impl ExceptionPortArrays
{
    pub(super) fn new() -> Self
    {
        Self {
            masks: [0; EXCEPTION_PORT_SLOTS],
            handlers: [MACH_PORT_NULL; EXCEPTION_PORT_SLOTS],
            behaviors: [0; EXCEPTION_PORT_SLOTS],
            flavors: [0; EXCEPTION_PORT_SLOTS],
            count: EXCEPTION_PORT_SLOTS as mach_msg_type_number_t,
        }
    }

    pub(super) fn entries(&self) -> Vec<ExceptionPortEntry>
    {
        (0..(self.count as usize).min(EXCEPTION_PORT_SLOTS))
            .map(|i| ExceptionPortEntry {
                mask: self.masks[i],
                handler: self.handlers[i],
                behavior: self.behaviors[i],
                flavor: self.flavors[i],
            })
            .collect()
    }
}

/// Read the exception ports of `task` for the types in `mask`.
///
/// The returned handlers are send rights the caller now owns (release them with
/// `mach_port_deallocate`).
#[cfg(target_os = "macos")]
pub(crate) fn get_exception_ports(
    task: mach_port_t,
    mask: exception_mask_t,
) -> std::result::Result<Vec<ExceptionPortEntry>, kern_return_t>
{
    let mut arrays = ExceptionPortArrays::new();
    // SAFETY: every array has `count` writable slots and outlives the call.
    let kr = unsafe {
        task_get_exception_ports(
            task,
            mask,
            arrays.masks.as_mut_ptr(),
            &mut arrays.count,
            arrays.handlers.as_mut_ptr(),
            arrays.behaviors.as_mut_ptr(),
            arrays.flavors.as_mut_ptr(),
        )
    };
    if kr == KERN_SUCCESS { Ok(arrays.entries()) } else { Err(kr) }
}

/// The exception ports a task had before the backend installed its handler.
///
/// Holds a send right for every saved handler and releases them when dropped;
/// [`restore`](Self::restore) puts the ports back first.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct SavedExceptionPorts
{
    pub(super) entries: Vec<ExceptionPortEntry>,
}

#[cfg(target_os = "macos")]
impl SavedExceptionPorts
{
    /// Install `port` as the handler of `task` for `mask` with `task_swap_exception_ports`,
    /// returning the ports it replaced.
    pub(crate) fn install(
        task: mach_port_t,
        mask: exception_mask_t,
        port: mach_port_t,
        behavior: exception_behavior_t,
        flavor: thread_state_flavor_t,
    ) -> std::result::Result<Self, kern_return_t>
    {
        let mut arrays = ExceptionPortArrays::new();
        // SAFETY: every array has `count` writable slots and outlives the call.
        let kr = unsafe {
            task_swap_exception_ports(
                task,
                mask,
                port,
                behavior,
                flavor,
                arrays.masks.as_mut_ptr(),
                &mut arrays.count,
                arrays.handlers.as_mut_ptr(),
                arrays.behaviors.as_mut_ptr(),
                arrays.flavors.as_mut_ptr(),
            )
        };
        if kr == KERN_SUCCESS {
            Ok(Self {
                entries: arrays.entries(),
            })
        } else {
            Err(kr)
        }
    }

    /// Put the saved exception ports back on `task` with `task_set_exception_ports`,
    /// one call per saved group. Every group is attempted even if an earlier one fails;
    /// the rights are released on drop. The test hook still restores the real handlers.
    pub(crate) fn restore(&self, task: mach_port_t, faults: &ReplyFaultInjector) -> BackendResult<()>
    {
        let mut failures = Vec::new();
        for entry in &self.entries {
            // SAFETY: `task` is a valid task send right and `entry.handler` a send right
            // this value owns (or NULL/DEAD); the call copies the right.
            let kr = unsafe { task_set_exception_ports(task, entry.mask, entry.handler, entry.behavior, entry.flavor) };
            if kr != KERN_SUCCESS {
                failures.push(format!("mask {:#x}: task_set_exception_ports returned {kr}", entry.mask));
            }
        }
        if faults.restore.swap(false, Ordering::SeqCst) {
            failures.push("exception-port restore failed (injected)".into());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(BackendError::Os {
                code: -1,
                message: format!("failed to restore exception ports: {}", failures.join("; ")),
            })
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SavedExceptionPorts
{
    fn drop(&mut self)
    {
        for entry in &self.entries {
            deallocate_right(entry.handler);
        }
    }
}

/// Receive one message from `exception_port`: block until one arrives, or with
/// `poll`, return `Err(MACH_RCV_TIMED_OUT)` at once when none is queued.
#[cfg(target_os = "macos")]
pub(super) fn receive_exception(
    exception_port: mach_port_t,
    poll: bool,
) -> std::result::Result<ReceiveBuffer, mach_msg_return_t>
{
    receive_exception_with_timeout(exception_port, poll.then_some(0))
}

/// Receive the next message other than a [`wake_loop`] wake, for the fault-injection
/// hooks that expect a real exception request next: a wake left queued by a `Continue`
/// the loop took before the wake arrived is skipped.
#[cfg(target_os = "macos")]
pub(super) fn receive_request_for_hook(exception_port: mach_port_t)
-> std::result::Result<ReceiveBuffer, mach_msg_return_t>
{
    loop {
        let buffer = receive_exception_with_timeout(exception_port, Some(1000))?;
        // SAFETY: the buffer holds a complete message that starts with its header.
        let header = unsafe { std::ptr::read_unaligned(buffer.0.as_ptr().cast::<mach_msg_header_t>()) };
        if header.msgh_id != WAKE_LOOP_ID {
            return Ok(buffer);
        }
    }
}

/// A bounded receive also lets the loop check commands while a stop is pending.
#[cfg(target_os = "macos")]
pub(super) fn receive_exception_with_timeout(
    exception_port: mach_port_t,
    timeout_ms: Option<u32>,
) -> std::result::Result<ReceiveBuffer, mach_msg_return_t>
{
    let mut buffer = MaybeUninit::<ReceiveBuffer>::zeroed();
    // No `MACH_RCV_LARGE`: an oversized message (never an exception request, which is
    // far smaller than the buffer) is destroyed instead of staying queued and spinning
    // the loop.
    let (options, timeout) = if let Some(timeout) = timeout_ms {
        (MACH_RCV_MSG | MACH_RCV_TIMEOUT, timeout)
    } else {
        (MACH_RCV_MSG, MACH_MSG_TIMEOUT_NONE)
    };
    // SAFETY: the buffer is writable for `size_of::<ReceiveBuffer>()` bytes and outlives
    // the call; the kernel writes at most that many.
    let kr = unsafe {
        mach_msg(
            buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
            options,
            0,
            std::mem::size_of::<ReceiveBuffer>() as mach_msg_size_t,
            exception_port,
            timeout,
            MACH_PORT_NULL,
        )
    };
    if kr == MACH_MSG_SUCCESS {
        // SAFETY: zeroed is a valid bit pattern for a byte array, and the kernel wrote a
        // complete message into it.
        Ok(unsafe { buffer.assume_init() })
    } else {
        Err(kr)
    }
}

/// Decode a received message as a `mach_exception_raise` request.
///
/// Anything else (another message id, or a request without its two port descriptors) is
/// destroyed, releasing every right it carries, and `None` is returned.
#[cfg(target_os = "macos")]
pub(super) fn decode_request(buffer: &ReceiveBuffer) -> Option<MachExceptionRaiseRequest>
{
    // SAFETY: the buffer holds a complete message and is larger than the request; the
    // read is unaligned-safe.
    let request = unsafe { std::ptr::read_unaligned(buffer.0.as_ptr().cast::<MachExceptionRaiseRequest>()) };
    let header = request.Head;
    let body = request.msgh_body;
    let is_exception = header.msgh_id == MACH_EXCEPTION_RAISE_ID
        && header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0
        && body.msgh_descriptor_count == 2;
    if is_exception {
        return Some(request);
    }
    if header.msgh_id == WAKE_LOOP_ID {
        // A header-only wake carries no rights; the loop goes back to its commands.
        return None;
    }
    warn!("Ignoring unexpected Mach message id {} on the exception port", header.msgh_id);
    let mut copy = ReceiveBuffer(buffer.0);
    // SAFETY: `copy` holds the received message, which owns its rights; destroying it
    // releases them once.
    unsafe { mach_msg_destroy(copy.0.as_mut_ptr().cast::<mach_msg_header_t>()) };
    None
}

/// Release one send right of this task, ignoring `MACH_PORT_NULL` and `MACH_PORT_DEAD`.
#[cfg(target_os = "macos")]
pub(super) fn deallocate_right(name: mach_port_t)
{
    if name == MACH_PORT_NULL || name == MACH_PORT_DEAD {
        return;
    }
    // SAFETY: the caller owns one user reference of `name`, released exactly once here.
    let kr = unsafe { crate::ffi::mach_port_deallocate(mach_task_self(), name) };
    if kr != KERN_SUCCESS {
        warn!("mach_port_deallocate({name}) failed: {kr}");
    }
}

/// Reply to every exception message still queued on `exception_port`, without blocking.
///
/// Threads that raised an exception after the loop collected its last stop wait in the
/// port's queue. On shutdown they must be answered too: destroying the port with a
/// message queued makes the kernel treat the exception as unhandled, which would kill a
/// target that a detach is supposed to leave running. The rights each message carries
/// are released.
#[cfg(target_os = "macos")]
pub(super) fn reply_to_queued_exceptions(exception_port: mach_port_t)
{
    while let Ok(buffer) = receive_exception(exception_port, true) {
        let Some(request) = decode_request(&buffer) else {
            continue;
        };
        reply_to_request(&request);
    }
}

/// Answer an unreported exception and release its carried send rights.
#[cfg(target_os = "macos")]
pub(super) fn reply_to_request(request: &MachExceptionRaiseRequest)
{
    if let Err(kr) = send_exception_reply(&request.Head) {
        error!("Failed to reply to a queued Mach exception: mach_msg returned {kr:#x}");
    }
    deallocate_right(request.thread.name);
    deallocate_right(request.task.name);
}

/// Send a reply to a Mach exception message.
///
/// After processing an exception, the debugger must send a reply to the
/// kernel indicating that the exception has been handled. This allows
/// the target process to continue execution.
///
/// Uses `mach_msg()` with `MACH_SEND_MSG` to send the reply. In a received message the
/// header's `msgh_remote_port` holds the kernel's send-once reply right, so that is where
/// the reply goes; its id is the request id plus 100. The send consumes that right. When
/// the send fails with `MACH_SEND_INVALID_DEST`, `mach_msg`
/// leaves the reply (and its right) with the caller, so the reply is destroyed here, as
/// `mach_msg_server` does. `MACH_SEND_TIMED_OUT` is unreachable: this send does
/// not request `MACH_SEND_TIMEOUT` (the timeout argument alone does not enable it).
///
/// See: [mach_msg(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_msg/)
#[cfg(target_os = "macos")]
pub(super) fn send_exception_reply(request: &mach_msg_header_t) -> std::result::Result<(), kern_return_t>
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
        // SAFETY: `NDR_record` is an immutable static provided by libsystem.
        NDR: unsafe { NDR_record },
        RetCode: KERN_SUCCESS,
    };

    // SAFETY: `reply` is a complete, correctly sized message that outlives the call.
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
        if kr == MACH_SEND_INVALID_DEST {
            // SAFETY: the send did not consume the message, so it still owns its reply
            // right; destroying it releases that right once.
            unsafe { mach_msg_destroy(&mut reply.Head) };
        }
        return Err(kr);
    }

    Ok(())
}

#[cfg(test)]
mod tests
{
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn restore_reports_every_failed_group()
    {
        use mach2::exception_types::{EXC_MASK_BREAKPOINT, EXC_MASK_SOFTWARE, EXCEPTION_DEFAULT};

        let saved = SavedExceptionPorts {
            entries: [EXC_MASK_BREAKPOINT, EXC_MASK_SOFTWARE]
                .into_iter()
                .map(|mask| ExceptionPortEntry {
                    mask,
                    handler: MACH_PORT_NULL,
                    behavior: EXCEPTION_DEFAULT as exception_behavior_t,
                    flavor: 0,
                })
                .collect(),
        };
        // A null task forces a real kernel error for every group. No port rights
        // are created; restoration must attempt and report both groups.
        let error = saved
            .restore(MACH_PORT_NULL, &ReplyFaultInjector::default())
            .expect_err("invalid task");
        let message = error.to_string();
        for entry in &saved.entries {
            assert!(message.contains(&format!("mask {:#x}:", entry.mask)), "{message}");
        }
    }

    /// No exception or fixture needed: a destroyed receive port leaves the extracted
    /// reply right as a dead name. Failed sending must release that name's reference.
    #[test]
    fn genuine_invalid_destination_releases_reply_right()
    {
        use mach2::mach_port::{mach_port_allocate, mach_port_destroy, mach_port_extract_right};
        use mach2::message::MACH_MSG_TYPE_MAKE_SEND_ONCE;
        use mach2::port::MACH_PORT_RIGHT_RECEIVE;

        let task = unsafe { mach_task_self() };
        let mut receive = MACH_PORT_NULL;
        let mut reply = MACH_PORT_NULL;
        let mut kind = 0;
        unsafe {
            assert_eq!(mach_port_allocate(task, MACH_PORT_RIGHT_RECEIVE, &mut receive), KERN_SUCCESS);
            assert_eq!(
                mach_port_extract_right(task, receive, MACH_MSG_TYPE_MAKE_SEND_ONCE, &mut reply, &mut kind),
                KERN_SUCCESS
            );
            assert_eq!(mach_port_destroy(task, receive), KERN_SUCCESS);
        }
        let mut port_type = 0;
        assert_eq!(
            unsafe { crate::ffi::mach_port_type(task, reply, &mut port_type) },
            KERN_SUCCESS
        );
        let request = mach_msg_header_t {
            msgh_bits: 0,
            msgh_size: 0,
            msgh_remote_port: reply,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: MACH_EXCEPTION_RAISE_ID,
        };
        assert_eq!(send_exception_reply(&request), Err(MACH_SEND_INVALID_DEST));
        assert_ne!(
            unsafe { crate::ffi::mach_port_type(task, reply, &mut port_type) },
            KERN_SUCCESS,
            "failed send leaked the reply's dead-name reference"
        );
    }
}
