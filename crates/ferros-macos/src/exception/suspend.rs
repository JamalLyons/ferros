//! Batches held behind a failed `task_suspend` (#191, #198).

#[allow(clippy::wildcard_imports)]
use super::*;

/// How long the loop receives before it retries a failed `task_suspend` the first
/// time (#191). Each further consecutive failure doubles it, up to
/// [`SUSPEND_RETRY_MAX_MS`] (#198).
#[cfg(target_os = "macos")]
pub(super) const SUSPEND_RETRY_MS: u32 = 10;

/// The longest delay between two retries of a failed `task_suspend`, and how long the
/// loop receives between passes once it has given up (#198).
#[cfg(target_os = "macos")]
pub(super) const SUSPEND_RETRY_MAX_MS: u32 = 500;

/// How long consecutive `task_suspend` failures may last before the loop gives up
/// retrying and reports the session unrecoverable (#198). A test can shorten it with
/// [`ReplyFaultInjector::set_suspend_retry_budget`].
pub const SUSPEND_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The retry state of a batch held behind failed `task_suspend`s (#191, #198).
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub(super) struct SuspendRetry
{
    /// Consecutive failed attempts; 0 while no batch is held.
    pub(super) failures: u32,
    /// When the first of them failed.
    pub(super) since: Option<std::time::Instant>,
    /// The last failure's code, reported for a batch held after the give-up.
    pub(super) last_kr: kern_return_t,
    /// When the next attempt is due; messages that arrive earlier do not advance it.
    pub(super) next_at: Option<std::time::Instant>,
    /// The failures outlasted the budget: the loop no longer calls `task_suspend`.
    pub(super) gave_up: bool,
}

#[cfg(target_os = "macos")]
impl SuspendRetry
{
    pub(super) fn failed(&mut self, kr: kern_return_t)
    {
        self.failures = self.failures.saturating_add(1);
        self.since.get_or_insert_with(std::time::Instant::now);
        self.last_kr = kr;
    }

    /// The delay before the next retry: [`SUSPEND_RETRY_MS`] doubled per failure after
    /// the first, at most [`SUSPEND_RETRY_MAX_MS`], which is also the pace after the
    /// give-up.
    pub(super) fn delay_ms(&self) -> u32
    {
        if self.gave_up {
            return SUSPEND_RETRY_MAX_MS;
        }
        // Six doublings already pass the ceiling; the bound keeps the shift in range.
        let doublings = self.failures.saturating_sub(1).min(6);
        (SUSPEND_RETRY_MS << doublings).min(SUSPEND_RETRY_MAX_MS)
    }

    /// Set the next attempt one [`Self::delay_ms`] from now.
    pub(super) fn schedule(&mut self)
    {
        self.next_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(u64::from(self.delay_ms())));
    }

    /// Whether the next attempt is due.
    pub(super) fn due(&self) -> bool
    {
        self.next_at.is_none_or(|next| std::time::Instant::now() >= next)
    }

    /// Milliseconds until the next attempt is due, rounded up; 0 once it is.
    pub(super) fn remaining_ms(&self) -> u32
    {
        self.next_at.map_or(0, |next| {
            let left = next.saturating_duration_since(std::time::Instant::now());
            u32::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(SUSPEND_RETRY_MAX_MS)
        })
    }

    pub(super) fn elapsed(&self) -> std::time::Duration
    {
        self.since.map_or(std::time::Duration::ZERO, |since| since.elapsed())
    }

    pub(super) fn exhausted(&self, budget: std::time::Duration) -> bool
    {
        self.elapsed() >= budget
    }
}

/// How long the loop receives, with no stop pending, while it keeps records for
/// threads a resume left suspended, before it checks its commands again.
#[cfg(target_os = "macos")]
pub(super) const KEPT_RECORDS_POLL_MS: u32 = 100;

/// The loop no longer holds a batch behind a failed `task_suspend` (#191), and has
/// published none of it. An interrupt that took the task suspend for that batch gets
/// its own stop instead, as `interrupt` would have reported it.
#[cfg(target_os = "macos")]
pub(super) fn end_unsuspended_hold(state: &mut ExceptionSharedState, events: &mpsc::Sender<LoopEvent>)
{
    state.held_unsuspended = false;
    if std::mem::take(&mut state.interrupt_suspend) && state.handshake_signals.is_none() {
        let _ = events.send(Ok(StopEvent::Interrupted { thread: None }));
    }
}

/// The `task_suspend` of a new stop failed (#191): other threads may still run, so no
/// stop of `stops` may be published, nor may the backend await a Continue. Returns the
/// records to retry, in order, as the new deferred queue.
///
/// Nothing is dropped. Each unanswered exception's thread is held with a
/// `thread_suspend` of its own, added to `frozen`, before its exception is answered
/// (a client Unix signal is suppressed first, as for any stop): a completed
/// single-step, a birth step-off or a newborn at its trap cannot run on, and no
/// unanswered reply blocks an external SIGKILL. If that suspend fails, the exception
/// stays unanswered instead, and an external SIGKILL does not complete while it is:
/// answering it would let the thread run past a stop nobody sees, so the freeze is
/// tried again on the next pass (#198), and one that succeeds answers the exception.
/// A client signal whose freeze succeeds but whose suppression fails stays unanswered
/// too, SIGKILL included, until a later pass suppresses it (and so answers it) or
/// teardown answers it.
/// Records already answered (signals deferred behind an
/// earlier hold, threads already in `frozen`) keep their only logical stop. The loop
/// retries the suspend and publishes them once it succeeds; shutdown answers them
/// like every deferred stop. During a private handshake, the private SIGSTOP stays
/// held in [`ExceptionSharedState::held`] with its reply, as `PT_DETACH` needs, and
/// its caller receives the failure at once instead of waiting for a stop that is not
/// coming; competing signals are preserved for it as on the success path. The first
/// failure of a run (`failures == 1`) is queued as an `Err` for the next `wait`; a pass
/// that attempted no counted suspend (after the give-up, before the backoff deadline,
/// or for a handshake) passes 0 and reports nothing. A dead task's
/// exceptions are answered at once: the exit watcher reports the terminal event.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(super) fn hold_unsuspended_stops(
    stops: Vec<(HeldException, StopEvent)>,
    kr: kern_return_t,
    failures: u32,
    state: &mut ExceptionSharedState,
    task: mach_port_t,
    pid: libc::pid_t,
    faults: &ReplyFaultInjector,
    events: &mpsc::Sender<LoopEvent>,
    frozen: &mut Vec<thread_act_t>,
) -> std::collections::VecDeque<(HeldException, StopEvent)>
{
    let failure = |count: usize| BackendError::Os {
        code: i64::from(kr),
        message: format!(
            "task_suspend failed with {kr} while handling a Mach exception; {count} stop(s) held unreported until the task \
             can be suspended"
        ),
    };
    let dead = state.dead || crate::lifecycle::task_is_dead(task);
    let mut held = std::collections::VecDeque::new();
    for (mut exception, event) in stops {
        let internal = is_internal_stop(&exception, &event, state);
        if dead {
            let _ = reply_to_held(&mut exception, faults);
            deallocate_right(exception.thread);
            if internal && let Some(ready) = state.internal_stop.take() {
                let _ = ready.send(Err(failure(1)));
            }
            continue;
        }
        if internal {
            // As on the success path: the handshake caller suppresses it on failure.
            state.soft_signals.push((exception.thread, libc::SIGSTOP));
            state.held.push(exception);
            let ready = state.internal_stop.take().expect("internal stop");
            let err = failure(1);
            error!("{err}");
            let _ = ready.send(Err(err));
            continue;
        }
        if state.handshake_signals.is_some() {
            if let Err(err) = preserve_handshake_stop(exception, &event, state, pid, faults) {
                let _ = events.send(Err(err));
            }
            continue;
        }
        if exception.header.is_some() {
            // A thread is frozen once per held batch: a retry finds it in `frozen`
            // with its exception still unanswered only if the suppression failed,
            // and then only tries that again, without reporting it again.
            let first = !frozen.contains(&exception.thread);
            if first && !faults.take_thread_freeze_failure() && freeze_thread(exception.thread) {
                frozen.push(exception.thread);
            } else if first && failures == 1 {
                error!(
                    "thread_suspend of thread {} failed after a failed task_suspend; its exception stays unanswered",
                    exception.thread
                );
            } else if first {
                debug!(
                    "thread_suspend of thread {} failed again; retried on the next pass",
                    exception.thread
                );
            }
            if frozen.contains(&exception.thread)
                && let Err(err) = prepare_stop(&mut exception, pid, faults)
                && first
            {
                let _ = events.send(Err(err));
            }
        }
        held.push_back((exception, event));
    }
    if !held.is_empty() {
        if failures == 1 {
            let err = failure(held.len());
            error!("{err}");
            if let Err(err) = events.send(Err(err)) {
                warn!("Failed to queue a task_suspend failure from Mach loop: {err}");
            }
        } else if failures > 0 {
            debug!("task_suspend retry {failures} failed with {kr}");
        }
    }
    held
}

/// Hold `thread` with a `thread_suspend` of its own and retain an independent right to
/// it, so its exception can be answered without letting it run (#191).
#[cfg(target_os = "macos")]
pub(super) fn freeze_thread(thread: thread_act_t) -> bool
{
    // SAFETY: the exception record owns a send right to this thread, so the name is
    // valid; this adds one user reference, released by `thaw_threads`.
    let kr = unsafe { mach2::mach_port::mach_port_mod_refs(mach_task_self(), thread, mach2::port::MACH_PORT_RIGHT_SEND, 1) };
    if kr != KERN_SUCCESS {
        return false;
    }
    // SAFETY: a tracee thread right just retained; `thaw_threads` balances the suspend.
    if unsafe { crate::ffi::thread_suspend(thread) } != KERN_SUCCESS {
        deallocate_right(thread);
        return false;
    }
    true
}

/// Balance every [`freeze_thread`] in `frozen` and release its right. A thread that has
/// exited is ignored.
#[cfg(target_os = "macos")]
pub(super) fn thaw_threads(frozen: &mut Vec<thread_act_t>)
{
    for thread in frozen.drain(..) {
        // SAFETY: `freeze_thread` suspended this thread once and retained the right.
        let kr = unsafe { crate::ffi::thread_resume(thread) };
        if kr != KERN_SUCCESS {
            debug!("thread_resume of a thread held after a failed task_suspend returned {kr}");
        }
        deallocate_right(thread);
    }
}
