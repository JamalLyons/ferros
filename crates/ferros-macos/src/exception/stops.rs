//! Turning received exceptions into stop records: births, handshakes, replies.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Release threads suspended so they could not pass a disarmed birth site.
pub(super) fn thaw_birth_freeze(state: &mut ExceptionSharedState) -> std::result::Result<(), String>
{
    let ports = state.birth.take_frozen();
    let mut resumed = Vec::new();
    let mut failed = Vec::new();
    for port in ports {
        // SAFETY: `prepare_birth_step_off` or the notifier path suspended this tracee
        // thread. This call only balances that one suspend.
        let kr = unsafe { crate::ffi::thread_resume(port) };
        if kr == KERN_SUCCESS || kr == mach2::kern_return::KERN_INVALID_NAME || kr == mach2::kern_return::KERN_TERMINATED {
            resumed.push(port);
        } else {
            failed.push(port);
        }
    }
    for port in state.birth.take_owned(&resumed) {
        deallocate_right(port);
    }
    if failed.is_empty() {
        return Ok(());
    }
    for port in failed {
        state.birth.freeze(port);
    }
    Err("thread_resume of a birth step-off freeze failed".into())
}

/// What [`try_consume_birth`] did with one exception.
pub(super) enum BirthConsume
{
    /// The reply was sent, or the stop stays internal.
    Consumed,
    /// Publish the stop. A reported birth breakpoint was already `thread_suspend`ed.
    Publish,
    /// `thread_suspend` failed and the reply is still outstanding.
    SuspendFailed,
}

/// Consume a birth-policy exception without publishing it.
///
/// [`BirthConsume::Consumed`] means the reply has been sent. A held newborn's thread
/// right stays in [`BirthPolicy::suspended`](crate::birth::BirthPolicy). A software
/// breakpoint on an entry is [`BirthConsume::Publish`] after the newborn is suspended.
#[cfg(target_os = "macos")]
pub(super) fn try_consume_birth(
    state: &mut ExceptionSharedState,
    exception: &mut HeldException,
    event: &StopEvent,
    task: mach_port_t,
    pid: libc::pid_t,
    faults: &ReplyFaultInjector,
    events: &mpsc::Sender<LoopEvent>,
) -> BirthConsume
{
    if state.birth.is_notifier(event) {
        return consume_image_notifier(state, exception, task, pid, faults, events);
    }
    if state.birth.is_step_off(event) {
        let thread = exception.thread;
        state.birth.take_step_off(thread);
        if let Err(err) = crate::registers::set_single_step(thread, false) {
            debug!("clearing a completed birth step-off on thread {thread} failed: {err}");
        }
        let remaining = state.birth.step_off_threads();
        let mut occupied = Vec::new();
        for port in remaining {
            match crate::task::read_registers_for_port(Architecture::Arm64, port) {
                Ok(registers) => occupied.push(registers.pc.value()),
                Err(err) => debug!("reading a birth step-off thread {port} failed: {err}"),
            }
        }
        if let Err(err) = state.birth.rearm_except(task, &occupied) {
            error!("failed to restore a new-thread trap: {err}");
            // Leave the exception unanswered and the peer freezes in place. The
            // client-stop path retries the restore and only releases those threads
            // after it succeeds. The task stays suspended either way.
            return BirthConsume::Publish;
        }
        if state.birth.step_off_threads().is_empty()
            && let Err(err) = thaw_birth_freeze(state)
        {
            let _ = events.send(Err(BackendError::Os { code: -1, message: err }));
            return BirthConsume::Publish;
        }
        if let Err(err) = reply_to_held(exception, faults) {
            let _ = events.send(Err(err));
        }
        deallocate_right(exception.thread);
        return BirthConsume::Consumed;
    }
    if let Some(engine_breakpoint) = state.birth.running_thread_entry(event, exception.thread) {
        return continue_running_thread(state, exception, task, faults, events, engine_breakpoint);
    }
    if state.birth.is_reported_birth(event) {
        return suspend_newborn(state, exception, faults, events, true);
    }
    if !state.birth.is_birth_breakpoint(event) {
        return BirthConsume::Publish;
    }
    suspend_newborn(state, exception, faults, events, false)
}

/// A thread this resume lets `Continue` entered an armed entry (a reused workqueue thread).
///
/// It is not a newborn and is not held. With an engine breakpoint there the stop is
/// published as that breakpoint. Otherwise its entry instruction is emulated and it runs
/// on; if that is impossible it is held like a newborn and the failure is reported.
pub(super) fn continue_running_thread(
    state: &mut ExceptionSharedState,
    exception: &mut HeldException,
    task: mach_port_t,
    faults: &ReplyFaultInjector,
    events: &mpsc::Sender<LoopEvent>,
    engine_breakpoint: bool,
) -> BirthConsume
{
    if engine_breakpoint {
        return BirthConsume::Publish;
    }
    if let Err(err) = state.birth.emulate_entry(task, exception.thread) {
        error!("a continuing thread entered a pthread entry and could not pass it: {err}");
        let _ = events.send(Err(crate::error::reword(err, |message| {
            format!("thread {} could not pass its pthread entry: {message}", exception.thread)
        })));
        return suspend_newborn(state, exception, faults, events, false);
    }
    if let Err(err) = reply_to_held(exception, faults) {
        let _ = events.send(Err(err));
    }
    deallocate_right(exception.thread);
    BirthConsume::Consumed
}

/// The main thread reached the executable entry: dyld has mapped the linked images.
/// Arm pthread entries before that entry runs. An engine breakpoint there is published.
pub(super) fn consume_image_notifier(
    state: &mut ExceptionSharedState,
    exception: &mut HeldException,
    task: mach_port_t,
    pid: libc::pid_t,
    faults: &ReplyFaultInjector,
    events: &mpsc::Sender<LoopEvent>,
) -> BirthConsume
{
    let engine_breakpoint = state.birth.notifier_is_engine_breakpoint();
    let covered = match state
        .birth
        .install_entries(task, ProcessId::from(pid as u32), exception.thread)
    {
        Ok(covered) => covered,
        Err(err) => {
            let message = match err {
                crate::birth::BirthInstallError::Unsupported(message) | crate::birth::BirthInstallError::Os(message) => {
                    message
                }
            };
            let _ = events.send(Err(BackendError::Os { code: -1, message }));
            return BirthConsume::SuspendFailed;
        }
    };
    if !covered {
        let _ = events.send(Err(BackendError::Os {
            code: -1,
            message: "libsystem_pthread was not loaded when the executable entry ran".into(),
        }));
        return BirthConsume::SuspendFailed;
    }
    if engine_breakpoint {
        return BirthConsume::Publish;
    }
    if let Err(err) = reply_to_held(exception, faults) {
        let _ = events.send(Err(err));
    }
    deallocate_right(exception.thread);
    BirthConsume::Consumed
}

/// `thread_suspend` a newborn at an armed entry.
///
/// `report` publishes the stop after the suspend (the engine has a breakpoint
/// there). Otherwise the stop stays internal.
pub(super) fn suspend_newborn(
    state: &mut ExceptionSharedState,
    exception: &mut HeldException,
    faults: &ReplyFaultInjector,
    events: &mpsc::Sender<LoopEvent>,
    report: bool,
) -> BirthConsume
{
    if report {
        // The published stop keeps the exception's own reference and releases it when
        // the stop is answered. The hold needs a reference of its own.
        // SAFETY: the exception message holds a send right to this thread, so the name
        // is valid; this adds one user reference, owned by `BirthPolicy::suspended`.
        let kr = unsafe {
            mach2::mach_port::mach_port_mod_refs(mach_task_self(), exception.thread, mach2::port::MACH_PORT_RIGHT_SEND, 1)
        };
        if kr != KERN_SUCCESS {
            let _ = events.send(Err(BackendError::Os {
                code: i64::from(kr),
                message: format!("retaining new thread {} failed: {kr}", exception.thread),
            }));
            return BirthConsume::SuspendFailed;
        }
    }
    // SAFETY: the exception message retains this thread right. One user suspend
    // keeps the newborn out of application code after the task runs again.
    let kr = if faults.fail_birth_suspend() {
        mach2::kern_return::KERN_FAILURE
    } else {
        unsafe { crate::ffi::thread_suspend(exception.thread) }
    };
    if kr != KERN_SUCCESS {
        error!("thread_suspend of a new thread {} failed: {kr}", exception.thread);
        if report {
            deallocate_right(exception.thread);
        }
        let _ = events.send(Err(BackendError::Os {
            code: i64::from(kr),
            message: format!("thread_suspend of a new thread {} failed: {kr}", exception.thread),
        }));
        return BirthConsume::SuspendFailed;
    }
    state.birth.hold(exception.thread);
    if report {
        return BirthConsume::Publish;
    }
    if let Err(err) = reply_to_held(exception, faults) {
        let _ = events.send(Err(err));
    }
    BirthConsume::Consumed
}

/// Turn a decoded request into a held exception and its stop event.
///
/// Reads the pc of the faulting thread for a breakpoint (the thread is blocked in the
/// kernel until the reply), then releases the task right the message carried. The thread
/// right is kept until the logical stop record is released.
#[cfg(target_os = "macos")]
///
/// `handshake` is [`ExceptionSharedState::handshake_generation`], read after the
/// receive: a handshake armed before then can be waiting for this exception, while every
/// record received earlier is a client record.
pub(super) fn hold_exception(
    request: &MachExceptionRaiseRequest,
    architecture: Architecture,
    handshake: u64,
) -> (HeldException, StopEvent)
{
    let thread_port = request.thread.name as thread_act_t;
    let code = request.code;
    let codes = [code[0], code[1]];
    let exception_type = request.exception;

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
    deallocate_right(request.task.name);

    let event = stop_event_from_exception(exception_type, ThreadId::from(u64::from(thread_port)), pc, codes);
    (
        HeldException {
            header: Some(request.Head),
            thread: thread_port,
            soft_signal: exception_type == EXC_SOFTWARE as exception_type_t && codes[0] == EXC_SOFT_SIGNAL,
            handshake,
        },
        event,
    )
}

/// Suppress a client signal before answering its exception. The task must be
/// suspended by the caller; retaining only the logical stop lets external SIGKILL
/// complete without allowing user code to run. A selected signal is queued afresh
/// on the same thread at resume, so XNU evaluates its default action correctly.
#[cfg(target_os = "macos")]
pub(super) fn prepare_stop(exception: &mut HeldException, pid: libc::pid_t, faults: &ReplyFaultInjector)
-> BackendResult<()>
{
    if exception.header.is_none() {
        return Ok(());
    }
    if exception.soft_signal {
        if faults.take_signal_suppression_failure() {
            return Err(BackendError::Os {
                code: i64::from(libc::ESRCH),
                message: "suppress signal before exception reply: injected failure".into(),
            });
        }
        // SAFETY: this is our tracee and the exception retains the thread right.
        let result = unsafe {
            crate::ffi::ptrace(
                crate::ffi::ptrace_requests::PT_THUPDATE,
                pid,
                exception.thread as usize as *mut libc::c_char,
                0,
            )
        };
        if result != 0 {
            let err = std::io::Error::last_os_error();
            return Err(BackendError::Os {
                code: i64::from(err.raw_os_error().unwrap_or(-1)),
                message: format!("suppress signal before exception reply: {err}"),
            });
        }
    }
    reply_to_held(exception, faults)
}

/// A handshake does not create client stops. Keep independent rights for application
/// signals so the backend can requeue them after the private stop has been consumed.
pub(super) fn preserve_handshake_stop(
    mut exception: HeldException,
    event: &StopEvent,
    state: &mut ExceptionSharedState,
    pid: libc::pid_t,
    faults: &ReplyFaultInjector,
) -> BackendResult<()>
{
    let result = (|| {
        if let StopEvent::Signal { signal, .. } = event
            && exception.soft_signal
        {
            let queued = RequeuedSignal::new(exception.thread, *signal)?;
            prepare_stop(&mut exception, pid, faults)?;
            state.handshake_signals.as_mut().expect("active handshake").push(queued);
        } else {
            reply_to_held(&mut exception, faults)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = reply_to_held(&mut exception, faults);
    }
    deallocate_right(exception.thread);
    result
}

/// Whether the backend holds the task suspended while stopped, outside a resume and a
/// private handshake, with no stop of the loop's own awaiting Continue.
///
/// An interrupt's suspend handed over for a held batch (#191) does not count: that
/// suspend is the batch's, whose stop the loop is about to publish.
pub(super) fn held_quietly(state: &ExceptionSharedState) -> bool
{
    state.suspends > 0
        && !state.preparing_resume
        && !state.pending_exception
        && state.handshake_signals.is_none()
        && !state.interrupt_suspend
}

/// The current [`ExceptionSharedState::handshake_generation`].
pub(super) fn current_generation(shared: &Mutex<ExceptionSharedState>) -> u64
{
    lock_shared(shared).handshake_generation
}

/// Whether `exception` is the private attach/detach SIGSTOP a handshake waits for.
///
/// Only a fresh kernel exception, whose reply is still outstanding, can be: `PT_DETACH`
/// needs the tracee waiting in it. A SIGSTOP record already answered (a client signal
/// suppressed during a stop, or held across a failed `task_suspend`, #191) is a client
/// signal, even while a handshake is waiting. So is one received before the handshake
/// armed, even if its suppression failed and its reply is still outstanding: only an
/// exception stamped with the current handshake generation can be the private stop.
pub(super) fn is_internal_stop(exception: &HeldException, event: &StopEvent, state: &ExceptionSharedState) -> bool
{
    exception.header.is_some()
        && exception.handshake == state.handshake_generation
        && matches!(
            event,
            StopEvent::Signal {
                signal: libc::SIGSTOP,
                ..
            }
        )
        && state.internal_stop.is_some()
}

/// Answer a requeued client signal without exposing another signal stop.
/// Returns true after consuming an internal request and its carried rights.
#[cfg(target_os = "macos")]
pub(super) fn reply_to_requeued_signal(
    request: &MachExceptionRaiseRequest,
    state: &mut ExceptionSharedState,
    pid: libc::pid_t,
    events: &mpsc::Sender<LoopEvent>,
    faults: &ReplyFaultInjector,
) -> bool
{
    let codes = request.code;
    if request.exception != EXC_SOFTWARE as exception_type_t || codes[0] != EXC_SOFT_SIGNAL {
        return false;
    }
    let Some(index) = state
        .requeued_signals
        .iter()
        .position(|queued| queued.thread == request.thread.name && i64::from(queued.signal) == codes[1])
    else {
        return false;
    };
    {
        let queued = state.requeued_signals.remove(index);
        // The fresh exception marks BSD state SSTOP. Select the now-correct signal
        // again to restore SRUN before replying, just as for a client-visible stop.
        // SAFETY: this loop belongs to the traced pid; the message and queued record
        // both retain the thread right. PT_THUPDATE treats addr as a port name.
        let result = unsafe {
            crate::ffi::ptrace(
                crate::ffi::ptrace_requests::PT_THUPDATE,
                pid,
                queued.thread as usize as *mut libc::c_char,
                queued.signal,
            )
        };
        if result != 0 {
            let err = std::io::Error::last_os_error();
            let _ = events.send(Err(BackendError::Os {
                code: i64::from(err.raw_os_error().unwrap_or(-1)),
                message: format!("select requeued signal: {err}"),
            }));
        }
    }
    deallocate_right(request.task.name);
    let mut exception = HeldException {
        header: Some(request.Head),
        thread: request.thread.name,
        soft_signal: true,
        handshake: state.handshake_generation,
    };
    // The fresh pass through issignal_locked evaluates this signal's own
    // properties rather than those cached before the original exception.
    if let Err(err) = reply_to_held(&mut exception, faults) {
        let _ = events.send(Err(err));
    }
    deallocate_right(exception.thread);
    true
}

/// Consume an outstanding reply; the caller retains ownership of the thread right.
///
/// With a failure injected through [`ReplyFaultInjector`], the reply is reported as
/// failed although it is still sent, so the thread does not stay blocked forever.
#[cfg(target_os = "macos")]
pub(super) fn reply_to_held(exception: &mut HeldException, faults: &ReplyFaultInjector) -> BackendResult<()>
{
    let Some(header) = exception.header.take() else {
        return Ok(());
    };
    let injected = faults.take();
    let sent = send_exception_reply(&header);
    let failure = match sent {
        Err(kr) => Some((kr, "")),
        Ok(()) if injected => Some((MACH_SEND_INVALID_DEST, " (injected by fail_next_exception_reply)")),
        Ok(()) => None,
    };
    match failure {
        None => Ok(()),
        Some((kr, note)) => Err(BackendError::Os {
            code: i64::from(kr),
            message: format!(
                "failed to reply to the Mach exception of thread {}: mach_msg returned {kr:#x}{note}; the thread may stay \
                 blocked",
                exception.thread
            ),
        }),
    }
}

#[cfg(test)]
mod tests
{
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn hardware_fault_is_not_requeued_as_a_unix_signal_during_a_handshake()
    {
        // A hardware exception maps to Fault for the portable event API; only
        // EXC_SOFT_SIGNAL denotes a Unix signal that can be replayed.
        // SAFETY: mach_thread_self returns an owned send right for this live thread.
        let thread = unsafe { mach2::mach_init::mach_thread_self() };
        let mut state = ExceptionSharedState::new();
        state.handshake_signals = Some(Vec::new());
        preserve_handshake_stop(
            HeldException {
                header: None,
                thread,
                soft_signal: false,
                handshake: 0,
            },
            &StopEvent::Fault {
                thread: ThreadId::from(u64::from(thread)),
                kind: FaultKind::MemoryAccess,
                detail: "EXC_BAD_ACCESS".into(),
            },
            &mut state,
            std::process::id() as libc::pid_t,
            &ReplyFaultInjector::default(),
        )
        .expect("release hardware exception");
        assert!(state.handshake_signals.as_ref().unwrap().is_empty());
    }

    /// A client SIGSTOP received before a detach handshake armed, whose reply is still
    /// outstanding (its suppression failed across failed `task_suspend`s, #191), is not
    /// that handshake's private stop; one received after it armed is.
    #[test]
    fn only_a_sigstop_received_after_the_handshake_armed_is_private()
    {
        // SAFETY: mach_thread_self returns an owned send right for this live thread.
        let thread = unsafe { mach2::mach_init::mach_thread_self() };
        let sigstop = StopEvent::Signal {
            thread: ThreadId::from(u64::from(thread)),
            signal: libc::SIGSTOP,
        };
        let header = mach_msg_header_t {
            msgh_bits: 0,
            msgh_size: 0,
            msgh_remote_port: MACH_PORT_NULL,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: MACH_EXCEPTION_RAISE_ID,
        };
        let mut state = ExceptionSharedState::new();
        let client = HeldException {
            header: Some(header),
            thread,
            soft_signal: true,
            handshake: state.handshake_generation,
        };
        let (ready, _wait) = mpsc::channel();
        state.arm_handshake(ready);
        let private = HeldException {
            header: Some(header),
            thread,
            soft_signal: true,
            handshake: state.handshake_generation,
        };
        assert!(
            !is_internal_stop(&client, &sigstop, &state),
            "a client SIGSTOP satisfied the handshake"
        );
        assert!(
            is_internal_stop(&private, &sigstop, &state),
            "the private SIGSTOP was not recognised"
        );
        deallocate_right(thread);
    }
}
