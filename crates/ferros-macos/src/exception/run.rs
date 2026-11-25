//! The exception loop and the release of held exceptions.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Release stop records for threads outside `keep`, answering any remaining replies.
///
/// Called under the shared lock after the caller selects signals. Retained records
/// keep their thread rights and signal decisions for a later resume. Failed replies
/// are reported through `events` unless death is already known.
#[cfg(target_os = "macos")]
pub(crate) fn answer_held_exceptions(
    state: &mut ExceptionSharedState,
    keep: &[thread_act_t],
    faults: &ReplyFaultInjector,
    events: &mpsc::Sender<LoopEvent>,
    task: mach_port_t,
)
{
    let (kept, answer): (Vec<HeldException>, Vec<HeldException>) = std::mem::take(&mut state.held)
        .into_iter()
        .partition(|exception| keep.contains(&exception.thread));
    for mut exception in answer {
        let result = reply_to_held(&mut exception, faults);
        deallocate_right(exception.thread);
        let Err(failure) = result else {
            continue;
        };
        if state.dead || crate::lifecycle::task_is_dead(task) {
            continue;
        }
        error!("{failure}");
        if let Err(err) = events.send(Err(failure)) {
            warn!("Failed to queue a reply failure from Mach loop: {err}");
        }
    }
    state
        .soft_signals
        .retain(|(port, _)| kept.iter().any(|exception| exception.thread == *port));
    state.held = kept;
}

/// Answer every held exception, kept ones included, and release their thread rights
/// (shutdown paths). Clears `pending_exception` and `soft_signals`.
#[cfg(target_os = "macos")]
pub(crate) fn release_held_exceptions(state: &mut ExceptionSharedState)
{
    for exception in std::mem::take(&mut state.held) {
        if let Some(header) = exception.header
            && let Err(err) = send_exception_reply(&header)
        {
            error!("Failed to reply to a held Mach exception on shutdown: mach_msg returned {err:#x}");
        }
        deallocate_right(exception.thread);
    }
    state.pending_exception = false;
    state.soft_signals.clear();
    state.requeued_signals.clear();
}

/// Receive Mach exceptions, suspend the task, suppress client Unix signals and answer
/// exceptions before publishing their stops. Retain logical signal decisions until
/// resume. While stopped, keep answering newly arriving exceptions so they cannot
/// block external SIGKILL. Private ptrace handshake signals retain their reply.
///
/// Commands are taken on every pass. Continue releases records of runnable threads and
/// answers their remaining replies; Shutdown answers all held replies, drains queued
/// messages and exits. Destroying
/// the receive port also exits the loop and releases retained thread rights.
///
/// See:
/// - [mach_msg(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_msg/)
/// - [Mach Exception Handling](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_exception_loop(
    exception_port: mach_port_t,
    task: mach_port_t,
    pid: libc::pid_t,
    commands: mpsc::Receiver<ExceptionLoopCommand>,
    shared: Arc<Mutex<ExceptionSharedState>>,
    architecture: Architecture,
    events: mpsc::Sender<LoopEvent>,
    faults: ReplyFaultInjector,
)
{
    let mut deferred: std::collections::VecDeque<(HeldException, StopEvent)> = std::collections::VecDeque::new();
    // Consecutive failed `task_suspend`s of the batch now held at the front of
    // `deferred` (#191), the backoff between them and whether the loop gave up (#198);
    // reset once a suspend succeeds or the batch is released.
    let mut retry = SuspendRetry::default();
    // Threads of that batch held with a `thread_suspend` (and a right) of their own.
    let mut frozen: Vec<thread_act_t> = Vec::new();
    loop {
        faults.park_loop_for_teardown();
        let (pending, handshake, handed_over, keeps_records) = {
            let state = lock_shared(&shared);
            (
                state.pending_exception,
                state.handshake_signals.is_some(),
                state.interrupt_suspend,
                !state.held.is_empty(),
            )
        };
        if !frozen.is_empty() && handshake {
            // A private handshake takes the deferred stops apart, and its SIGSTOP
            // must not pick a thread that stays suspended.
            thaw_threads(&mut frozen);
        }
        // Commands are taken in every state, not only while a stop is pending: a
        // shutdown must also reach a loop that retries a failed suspend or keeps
        // records, so it answers what is queued before the port is destroyed (#191).
        // A Continue is only ever sent for a pending stop.
        match commands.try_recv() {
            Ok(ExceptionLoopCommand::Continue {
                ack,
                keep,
                discard_deferred,
            }) => {
                let mut state = lock_shared(&shared);
                if discard_deferred {
                    // The task is still suspended: the backend resumes it only
                    // after this command is acknowledged.
                    thaw_threads(&mut frozen);
                    for (mut exception, _) in deferred.drain(..) {
                        let _ = reply_to_held(&mut exception, &faults);
                        deallocate_right(exception.thread);
                    }
                    retry = SuspendRetry::default();
                    end_unsuspended_hold(&mut state, &events);
                }
                answer_held_exceptions(&mut state, &keep, &faults, &events, task);
                state.pending_exception = false;
                faults.pending_stop.store(false, Ordering::SeqCst);
                let _ = ack.send(());
                continue;
            }
            Ok(ExceptionLoopCommand::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => {
                release_held_exceptions(&mut lock_shared(&shared));
                reply_to_queued_exceptions(exception_port);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // A pass before the backoff deadline (#198): an exception arrived during the wait.
        // It is held at once, its thread frozen and its exception answered, so it does
        // not hold back an external SIGKILL, but no `task_suspend` is attempted for it.
        let mut early = false;
        if retry.failures > 0 && !pending {
            // Give the kernel a moment before retrying, backing off while the failures
            // go on, and take any exception raised meanwhile, so a task that cannot be
            // suspended does not make the loop spin. The deadline of the next attempt
            // does not move when a message arrives: an exception is held without an
            // attempt, and a wake only ends the receive, so the loop takes its command
            // and waits out the rest. An interrupt that took the task suspend for the
            // held batch needs no wait, and a private handshake, which takes the held
            // batch apart, no backed-off one. After giving up, the loop only services
            // its port and commands at the longest delay.
            let timeout = if handed_over {
                0
            } else if handshake {
                SUSPEND_RETRY_MS
            } else {
                retry.remaining_ms()
            };
            let mut arrived = false;
            match receive_exception_with_timeout(exception_port, Some(timeout)) {
                Ok(buffer) => {
                    if let Some(request) = decode_request(&buffer) {
                        let mut state = lock_shared(&shared);
                        if !reply_to_requeued_signal(&request, &mut state, pid, &events, &faults) {
                            let generation = state.handshake_generation;
                            drop(state);
                            deferred.push_back(hold_exception(&request, architecture, generation));
                            arrived = true;
                        }
                    }
                }
                Err(kr) if kr == MACH_RCV_PORT_DIED || kr == MACH_RCV_INVALID_NAME => {
                    debug!("Mach exception port closed, exiting handler loop");
                    break;
                }
                Err(_) => {}
            }
            if deferred.is_empty() {
                thaw_threads(&mut frozen);
                retry = SuspendRetry::default();
                end_unsuspended_hold(&mut lock_shared(&shared), &events);
            } else if !handed_over && !handshake && !retry.due() {
                if !arrived {
                    // A wake or nothing: take the command, then wait out the rest.
                    continue;
                }
                early = true;
            }
        }
        // A deferred stop that must wait again goes back to the front, keeping the
        // order in which the signals arrived.
        let popped = !pending && !deferred.is_empty();
        let mut first = if popped {
            deferred.pop_front().expect("deferred request")
        } else {
            // While it keeps records, shutdown joins the loop: bound the receive, so the
            // join does not depend on the wake alone.
            let timeout = if pending {
                Some(10)
            } else {
                keeps_records.then_some(KEPT_RECORDS_POLL_MS)
            };
            let buffer = match receive_exception_with_timeout(exception_port, timeout) {
                Ok(buffer) => buffer,
                Err(kr) if kr == MACH_RCV_PORT_DIED || kr == MACH_RCV_INVALID_NAME => {
                    debug!("Mach exception port closed, exiting handler loop");
                    break;
                }
                Err(_) => continue,
            };
            let Some(request) = decode_request(&buffer) else {
                continue;
            };
            let generation = {
                let mut state = lock_shared(&shared);
                if reply_to_requeued_signal(&request, &mut state, pid, &events, &faults) {
                    continue;
                }
                state.handshake_generation
            };
            hold_exception(&request, architecture, generation)
        };
        let mut injected_batch = Vec::new();
        // Only a step that starts a new stop outside a ptrace handshake: those paths
        // `continue` before publishing and would drop the batched peer exception.
        if !pending && matches!(first.1, StopEvent::SingleStep { .. }) && lock_shared(&shared).handshake_signals.is_none() {
            let peer = faults.step_batch_signal.swap(0, Ordering::SeqCst);
            if peer != 0 {
                // SAFETY: this armed test hook requires a peer with exactly one
                // backend-owned `thread_suspend` (from a step request) and a SIGUSR1
                // handler; the backend holds the thread right. Queue the signal first
                // so no user instruction is needed to raise it.
                assert_eq!(unsafe { crate::ffi::__pthread_kill(peer, libc::SIGUSR1) }, 0);
                assert_eq!(unsafe { crate::ffi::thread_resume(peer) }, KERN_SUCCESS);
                let buffer = receive_request_for_hook(exception_port).expect("signal batched with real step");
                let request = decode_request(&buffer).expect("real signal request");
                let racing = hold_exception(&request, architecture, current_generation(&shared));
                assert!(
                    matches!(racing.1, StopEvent::Signal { thread, signal: libc::SIGUSR1 }
                    if thread.raw() == u64::from(peer)),
                    "unexpected batched event: {:?}",
                    racing.1
                );
                // SAFETY: restore exactly the one suspension temporarily balanced above.
                assert_eq!(unsafe { crate::ffi::thread_suspend(peer) }, KERN_SUCCESS);
                injected_batch.push(racing);
            }
        }

        if is_internal_stop(&first.0, &first.1, &lock_shared(&shared)) {
            let race = faults.private_stop_race.swap(0, Ordering::SeqCst);
            if race != 0 {
                // Consume the first private stop, then arrange a fresh private
                // SIGSTOP behind a real application signal. This lets the test force
                // either order without depending on scheduler timing.
                prepare_stop(&mut first.0, pid, &faults).expect("consume initial private stop");
                let ports = crate::threads::ThreadManager::refresh_thread_list(task, &[])
                    .expect("enumerate threads for private-stop race");
                let other = *ports
                    .iter()
                    .find(|&&port| port != first.0.thread)
                    .expect("private-stop race fixture needs another thread");
                deallocate_right(first.0.thread);
                // SAFETY: refresh_thread_list retained this tracee thread right.
                assert_eq!(unsafe { crate::ffi::__pthread_kill(other, libc::SIGUSR1) }, 0);
                let buffer = receive_request_for_hook(exception_port).expect("racing signal exception");
                let request = decode_request(&buffer).expect("racing exception request");
                let mut racing = hold_exception(&request, architecture, current_generation(&shared));
                for port in ports {
                    deallocate_right(port);
                }
                if race == 2 {
                    prepare_stop(&mut racing.0, pid, &faults).expect("prepare same-batch signal");
                }
                // SAFETY: the original private stop's tracee still belongs to us.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
                if race == 1 {
                    first = racing;
                } else {
                    let buffer = receive_request_for_hook(exception_port).expect("fresh private stop");
                    let request = decode_request(&buffer).expect("private exception request");
                    first = hold_exception(&request, architecture, current_generation(&shared));
                    injected_batch.push(racing);
                }
            }
        }
        {
            let mut state = lock_shared(&shared);
            if state.handshake_signals.is_some() && !is_internal_stop(&first.0, &first.1, &state) {
                // Do not suspend the task for a stop that won the race against our
                // SIGSTOP: doing so prevents that private stop from ever arriving.
                if let Err(err) = preserve_handshake_stop(first.0, &first.1, &mut state, pid, &faults) {
                    let _ = events.send(Err(err));
                }
                continue;
            }
        }
        if !pending
            && faults
                .stop_publication
                .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let deadline = std::time::Instant::now() + faults.stop_publication_deadline();
            while faults.stop_publication.load(Ordering::SeqCst) == 2 {
                assert!(std::time::Instant::now() < deadline, "paused stop was not released");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            faults.stop_publication.store(0, Ordering::SeqCst);
        }
        if !pending {
            // #79: the backend holds the task stopped (after launch, attach or an
            // interrupt) without a reported stop awaiting Continue. A Unix signal the
            // kernel raises now was sent while stopped: like one arriving during a
            // reported stop, answer it early and defer its logical stop until resume.
            // Publishing it here would race the client's resume, which must refuse a
            // stop queued after its drain. Once a resume is preparing, a new stop is
            // still published and refused (G8): its decision cannot be taken yet.
            let mut state = lock_shared(&shared);
            if held_quietly(&state) && first.0.soft_signal && !is_internal_stop(&first.0, &first.1, &state) {
                let (mut exception, event) = first;
                if let Err(err) = prepare_stop(&mut exception, pid, &faults) {
                    let _ = events.send(Err(err));
                }
                if popped {
                    deferred.push_front((exception, event));
                } else {
                    deferred.push_back((exception, event));
                }
                state.pending_exception = true;
                continue;
            }
        }
        if pending {
            // Preserve the next logical stop, but not its kernel reply: otherwise an
            // ordinary signal arriving at a breakpoint could block a later SIGKILL.
            // A birth-policy breakpoint or its internal step-off is not a client stop:
            // the task is already suspended, so the newborn is held or the step is
            // retired without queueing an event beside the stop already published.
            let (mut exception, event) = first;
            let mut state = lock_shared(&shared);
            match try_consume_birth(&mut state, &mut exception, &event, task, pid, &faults, &events) {
                BirthConsume::Consumed => continue,
                BirthConsume::SuspendFailed => {
                    state.held.push(exception);
                    continue;
                }
                BirthConsume::Publish => {}
            }
            if (state.suspends > 0 || state.preparing_resume)
                && !is_internal_stop(&exception, &event, &state)
                && let Err(err) = prepare_stop(&mut exception, pid, &faults)
            {
                let _ = events.send(Err(err));
            }
            deferred.push_back((exception, event));
            continue;
        }

        {
            // A test can pause after a birth step-off is received and before this
            // task_suspend. Other threads keep running during that pause unless the
            // resume already suspended them. Production resumes do not arm the hold.
            let step_off = lock_shared(&shared).birth.is_step_off(&first.1);
            faults.pause_for_birth_step_off(step_off);
            faults.pause_suspend_retry(popped && retry.failures > 0 && !retry.gave_up && !early);

            // Stop every thread, collect every message of this stop, mark the exceptions
            // as held and queue the events in one step under the lock, so a concurrent
            // resume or interrupt sees either none or all of this stop.
            let mut state = lock_shared(&shared);
            faults.suspend_retry_locked();
            if state.handshake_signals.is_some() && !is_internal_stop(&first.0, &first.1, &state) {
                // A handshake armed since the check above (#191): its teardown has
                // balanced, or will balance, every suspend the backend counts, an
                // interrupt's handed-over one included, and its private SIGSTOP may not
                // have been raised yet. A fresh task suspend for a client stop would keep
                // it from ever arriving. Preserve the stop for the handshake as above, and
                // release the threads held for a failed suspend, which the kernel may have
                // chosen to take that SIGSTOP.
                state.interrupt_suspend = false;
                thaw_threads(&mut frozen);
                for (exception, event) in std::iter::once(first).chain(injected_batch) {
                    if let Err(err) = preserve_handshake_stop(exception, &event, &mut state, pid, &faults) {
                        let _ = events.send(Err(err));
                    }
                }
                continue;
            }
            // An interrupt that found a held batch took the task suspend for it, already
            // counted in `suspends` (#191): publish the batch with it as the stop.
            // A handshake's teardown has already balanced that suspend with the others
            // (`release_suspends`), so its private SIGSTOP takes a suspend of its own.
            let handover = std::mem::take(&mut state.interrupt_suspend) && state.handshake_signals.is_none();
            // After the give-up (#198) no more `task_suspend`s are taken for client
            // stops: what arrives is held like the batch, its thread frozen and its
            // exception answered. The same holds before the backoff deadline. A private
            // handshake's SIGSTOP still gets one, so a detach can complete its handshake
            // if the task can be suspended again.
            let skipped = (retry.gave_up || early) && state.handshake_signals.is_none();
            let kr = if handover {
                KERN_SUCCESS
            } else if skipped {
                retry.last_kr
            } else {
                faults.exception_suspend_attempts.fetch_add(1, Ordering::SeqCst);
                if faults.take_exception_suspend_failure() {
                    KERN_FAILURE
                } else {
                    // SAFETY: `task` is the send right the backend holds for as long as
                    // this loop runs.
                    unsafe { task_suspend(task) }
                }
            };
            if kr == KERN_SUCCESS && !handover {
                state.suspends += 1;
            }

            let mut stops = vec![first];
            stops.extend(injected_batch);
            stops.extend(deferred.drain(..));
            // `task_suspend` returned, so threads that raised an exception at the same
            // moment have queued their messages: take them without blocking.
            while let Ok(buffer) = receive_exception(exception_port, true) {
                if let Some(request) = decode_request(&buffer)
                    && !reply_to_requeued_signal(&request, &mut state, pid, &events, &faults)
                {
                    stops.push(hold_exception(&request, architecture, state.handshake_generation));
                }
            }
            if kr != KERN_SUCCESS {
                // A handshake takes the held batch apart: a failed suspend for its
                // private SIGSTOP is reported to the handshake caller, and does not count
                // against the old batch's retry budget.
                let counted = !skipped && state.handshake_signals.is_none();
                if counted {
                    retry.failed(kr);
                }
                // A pass that attempted no counted suspend (after the give-up, before the
                // backoff deadline, or for a handshake) passes 0: it reports nothing again.
                let held = hold_unsuspended_stops(
                    stops,
                    kr,
                    if counted { retry.failures } else { 0 },
                    &mut state,
                    task,
                    pid,
                    &faults,
                    &events,
                    &mut frozen,
                );
                if held.is_empty() {
                    retry = SuspendRetry::default();
                } else if !early {
                    // The next attempt (or, after the give-up, the next pass) is due one
                    // backoff delay from now, however many messages arrive meanwhile.
                    retry.schedule();
                }
                if !held.is_empty() && counted && !retry.gave_up && retry.exhausted(faults.suspend_retry_budget()) {
                    retry.gave_up = true;
                    let err = BackendError::Os {
                        code: i64::from(kr),
                        message: format!(
                            "task_suspend kept failing with {kr} for {:?} ({} attempts) while handling a Mach exception; \
                             the task cannot be suspended and the session is unrecoverable: {} stop(s) stay held, detach \
                             or kill the target",
                            retry.elapsed(),
                            retry.failures,
                            held.len()
                        ),
                    };
                    error!("{err}");
                    if let Err(err) = events.send(Err(err)) {
                        warn!("Failed to queue the task_suspend give-up from Mach loop: {err}");
                    }
                }
                state.held_unsuspended = !held.is_empty();
                deferred = held;
                continue;
            }
            retry = SuspendRetry::default();
            state.held_unsuspended = false;
            // The task suspension now holds the threads of an earlier failed batch.
            thaw_threads(&mut frozen);
            if stops.len() > 1 {
                debug!("{} threads stopped at once", stops.len());
            }

            let mut client = Vec::new();
            let mut stuck = Vec::new();
            for (mut exception, event) in stops {
                match try_consume_birth(&mut state, &mut exception, &event, task, pid, &faults, &events) {
                    BirthConsume::Consumed => continue,
                    BirthConsume::SuspendFailed => stuck.push(exception),
                    BirthConsume::Publish => client.push((exception, event)),
                }
            }
            // Only birth-policy stops: the newborn is `thread_suspend`ed (or has stepped
            // off the trap, which releases threads frozen for that step) and the task
            // runs again. Do not publish a stop.
            if client.is_empty() {
                if !stuck.is_empty() {
                    // The newborn is still inside its exception. Keep the task
                    // suspended and surface a stop so resume can try the suspend again.
                    state.held.extend(stuck);
                    state.pending_exception = true;
                    faults.pending_stop.store(true, Ordering::SeqCst);
                    let _ = events.send(Ok(StopEvent::Interrupted { thread: None }));
                    continue;
                }
                if handover {
                    // Nothing of the held batch is a client stop: the interrupt's
                    // suspend stays, and its stop is reported as for any interrupt.
                    let _ = events.send(Ok(StopEvent::Interrupted { thread: None }));
                    continue;
                }
                if kr == KERN_SUCCESS {
                    // SAFETY: this balances the `task_suspend` just taken above.
                    let resume_kr = unsafe { task_resume(task) };
                    if resume_kr == KERN_SUCCESS {
                        state.suspends = state.suspends.saturating_sub(1);
                    } else {
                        error!("task_resume failed after a new-thread hold: {resume_kr}");
                        state.pending_exception = true;
                        faults.pending_stop.store(true, Ordering::SeqCst);
                        let _ = events.send(Ok(StopEvent::Interrupted { thread: None }));
                    }
                }
                continue;
            }
            state.held.extend(stuck);
            // A real stop cancels an in-flight step off a birth site. The task stays
            // suspended. Peers frozen for that step are released only after the trap
            // is armed again; a failed restore leaves them suspended.
            for thread in state.birth.clear_step_offs() {
                if let Err(err) = crate::registers::set_single_step(thread, false) {
                    debug!("clearing a birth step-off bit on thread {thread} failed: {err}");
                }
            }
            if let Err(err) = state.birth.rearm_except(task, &[]) {
                error!("failed to restore a new-thread trap: {err}");
                let _ = events.send(Err(BackendError::Os {
                    code: -1,
                    message: format!("failed to restore the new-thread trap: {err}"),
                }));
            } else if let Err(err) = thaw_birth_freeze(&mut state) {
                let _ = events.send(Err(BackendError::Os { code: -1, message: err }));
            }

            state.pending_exception = true;
            faults.pending_stop.store(true, Ordering::SeqCst);
            for (mut exception, event) in client {
                let internal = is_internal_stop(&exception, &event, &state);
                if state.handshake_signals.is_some() && !internal {
                    if let Err(err) = preserve_handshake_stop(exception, &event, &mut state, pid, &faults) {
                        let _ = events.send(Err(err));
                    }
                    continue;
                }
                // Private attach/detach signals retain their reply for the ptrace
                // handshake. All client signals are suppressed now and requeued only
                // if selected on resume, with the task suspension holding user code.
                let failure = if !internal && state.suspends > 0 {
                    prepare_stop(&mut exception, pid, &faults).err()
                } else {
                    None
                };
                if let StopEvent::Signal { signal, .. } = event
                    && exception.soft_signal
                {
                    state.soft_signals.push((exception.thread, signal));
                }
                state.held.push(exception);
                if internal && let Some(ready) = state.internal_stop.take() {
                    let _ = ready.send(Ok(()));
                    continue;
                }
                if state.dead {
                    continue;
                }
                if let Err(err) = events.send(Ok(event)) {
                    warn!("Failed to send stop event from Mach loop: {err}");
                }
                if let Some(err) = failure {
                    let _ = events.send(Err(err));
                }
            }
        }
    }
    thaw_threads(&mut frozen);
    for (mut exception, _) in deferred {
        let _ = reply_to_held(&mut exception, &faults);
        deallocate_right(exception.thread);
    }
    {
        // The backend balances an interrupt's suspend with the others on teardown.
        let mut state = lock_shared(&shared);
        state.held_unsuspended = false;
        state.interrupt_suspend = false;
    }
    // The port was destroyed while the loop was blocked receiving: exceptions kept for
    // suspended threads are still held. Answer them, so those threads are not left
    // blocked in the kernel, and release their rights.
    release_held_exceptions(&mut lock_shared(&shared));
}
