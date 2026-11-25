//! Test-only fault injection for the exception loop.

use std::time::Duration;

#[allow(clippy::wildcard_imports)]
use super::*;

/// How long a paused stop publication waits for release unless a test sets a deadline.
pub const DEFAULT_STOP_PUBLICATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Fault injection for tests: makes the exception loop report the next reply as failed.
///
/// Obtained from [`MacOSBackend::reply_fault_injector`](crate::MacOSBackend::reply_fault_injector);
/// see there. Not part of the supported API.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct ReplyFaultInjector
{
    pub(super) reply: Arc<AtomicBool>,
    pub(crate) resume: Arc<AtomicBool>,
    pub(super) restore: Arc<AtomicBool>,
    pub(super) private_stop_race: Arc<AtomicI32>,
    pub(super) pending_stop: Arc<AtomicBool>,
    /// 0 idle, 1 armed, 2 paused in the exception loop, 3 released by the test.
    pub(super) stop_publication: Arc<AtomicU8>,
    /// How long a paused publication waits for release before the loop panics, in
    /// milliseconds; 0 means [`DEFAULT_STOP_PUBLICATION_DEADLINE`]. Tests only.
    pub(super) stop_publication_deadline: Arc<AtomicU64>,
    /// Report `threads` sorted by thread port name, so a test can rely on the order
    /// Session steps breakpoint sites in. Tests only (#183).
    pub(crate) sorted_threads: Arc<AtomicBool>,
    pub(super) publish_during_resume: Arc<AtomicBool>,
    pub(super) step_batch_signal: Arc<std::sync::atomic::AtomicU32>,
    pub(crate) other_exit_reaper: Arc<AtomicBool>,
    pub(crate) handshake_requeue: Arc<AtomicBool>,
    pub(crate) detach_handshake: Arc<AtomicBool>,
    detach_stop_wait: Arc<Mutex<Option<DetachStopWait>>>,
    pub(super) detach_suspend_probe: Arc<Mutex<Option<DetachSuspendProbe>>>,
    /// 0 idle, 1 armed, 2 paused in the exception loop, 3 released by the test.
    pub(super) birth_step_off_hold: Arc<AtomicU8>,
    /// Fail the next `thread_suspend` of a newborn. Tests only.
    pub(super) birth_suspend: Arc<AtomicBool>,
    /// How many of the exception loop's next `task_suspend`s for a new stop fail. Tests only.
    pub(super) exception_suspend: Arc<std::sync::atomic::AtomicU32>,
    /// How many of the next client-signal suppressions (`PT_THUPDATE(0)`) fail. Tests only.
    pub(super) signal_suppression: Arc<std::sync::atomic::AtomicU32>,
    /// 0 idle, 1 armed, 2 paused before a suspend retry, 3 released by a detach
    /// handshake. Tests only (#191).
    pub(super) suspend_retry_hold: Arc<AtomicU8>,
    /// The retry budget of a failed exception `task_suspend` in milliseconds; 0 means
    /// [`SUSPEND_RETRY_BUDGET`]. Tests only (#198).
    pub(super) suspend_retry_budget: Arc<AtomicU64>,
    /// How many `task_suspend`s the exception loop has attempted for a new stop,
    /// injected failures included. Counted always, read by tests only (#198).
    pub(super) exception_suspend_attempts: Arc<std::sync::atomic::AtomicU32>,
    /// How many of the next [`freeze_thread`]s fail without calling the kernel. Tests only (#198).
    pub(super) thread_freeze: Arc<std::sync::atomic::AtomicU32>,
    /// 0 idle, 1 armed, 2 paused before a suspend retry, 3 released by a detach that
    /// armed its handshake, 4 the loop holds the shared lock for that retry. Tests only (#198).
    pub(super) detach_stop_hold: Arc<AtomicU8>,
    /// Receives whether the handshake still awaited its private stop when the detach
    /// held by [`Self::send_next_detach_stop_after_suspend_retry`] sent it. Tests only (#198).
    pub(super) detach_stop_probe: Arc<Mutex<Option<mpsc::Sender<bool>>>>,
    /// Every exception `task_suspend` for a new stop and every client-signal suppression
    /// fail until the next detach arms its handshake. Tests only (#198).
    pub(super) fail_until_detach: Arc<AtomicBool>,
    /// Queue a real exception on the port at the next teardown. Tests only (#198, F6).
    pub(super) teardown_exception: Arc<AtomicBool>,
    /// Set once that exception is queued on the port, unanswered. Tests only (#198).
    pub(super) teardown_exception_queued: Arc<AtomicBool>,
    /// 0 idle, 1 requested by a teardown, 2 parked at the top of a loop pass, 3 released.
    /// Tests only (#198).
    pub(super) loop_park: Arc<AtomicU8>,
    /// Fail the next signal selection of a resume before it touches the kernel. Tests only (#250).
    pub(crate) signal_selection: Arc<AtomicBool>,
    /// Drop the acknowledgement of the next Continue the loop handles, after handling it.
    /// Tests only (#250).
    pub(super) continue_ack: Arc<AtomicBool>,
    /// The acknowledgement withheld by `continue_ack`, kept so the resume times out.
    pub(super) withheld_ack: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    /// The next resume takes one extra task suspend, its last `task_resume` resumes the
    /// task and then reports failure, and the re-suspend fails. Tests only (#250).
    pub(crate) resume_partway: Arc<AtomicBool>,
}

/// Overrides the next detach's private stop waits. Tests only (#149, #282).
#[derive(Debug)]
pub(crate) struct DetachStopWait
{
    pub(crate) recheck: Duration,
    /// If set, hide the stop from detach but report whether it really landed before
    /// SIGCONT. Otherwise, force the first wait to expire before the reply.
    pub(crate) before_continue: Option<(Duration, mpsc::Sender<bool>)>,
}

#[derive(Debug)]
pub(super) struct DetachSuspendProbe
{
    pub(super) thread: ThreadId,
    pub(super) sample: mpsc::Sender<ferros_core::error::Result<i32>>,
}

impl ReplyFaultInjector
{
    /// Make the next exception reply report a failure.
    ///
    /// See [`MacOSBackend::fail_next_exception_reply`](crate::MacOSBackend::fail_next_exception_reply).
    pub fn fail_next_exception_reply(&self)
    {
        self.reply.store(true, Ordering::SeqCst);
    }

    /// Fail the next task resume before calling the kernel; the task stays suspended.
    pub fn fail_next_task_resume(&self)
    {
        self.resume.store(true, Ordering::SeqCst);
    }

    /// Report failure for the next exception-port restore, after really restoring it.
    pub fn fail_next_exception_port_restore(&self)
    {
        self.restore.store(true, Ordering::SeqCst);
    }

    /// Fail the next detach handshake after its private signal has arrived, so
    /// cleanup must use the real fallback PT_DETACH. Tests only.
    pub fn fail_next_detach_handshake(&self)
    {
        self.detach_handshake.store(true, Ordering::SeqCst);
    }

    /// Make the next detach behave as if its wait for the private SIGSTOP timed out:
    /// its SIGCONT is sent before the stop is even answered, so the stop lands after
    /// it, as for a target not scheduled for over a second. `recheck` bounds the
    /// subsequent wait for the late stop. Tests only (#149).
    pub fn expire_next_detach_stop_wait(&self, recheck: Duration)
    {
        *self.detach_stop_wait.lock().unwrap_or_else(PoisonError::into_inner) = Some(DetachStopWait {
            recheck,
            before_continue: None,
        });
    }

    /// Make the next detach never observe its private SIGSTOP taking effect, so it
    /// must report that the released process may stay stopped. Skip both detach waits
    /// and independently report whether the real stop took effect before SIGCONT,
    /// polling for up to `probe_timeout`. Tests only (#149, #282).
    pub fn hide_next_detach_stop(&self, probe_timeout: Duration) -> mpsc::Receiver<bool>
    {
        let (probe, result) = mpsc::channel();
        *self.detach_stop_wait.lock().unwrap_or_else(PoisonError::into_inner) = Some(DetachStopWait {
            recheck: Duration::ZERO,
            before_continue: Some((probe_timeout, probe)),
        });
        result
    }

    pub(crate) fn take_detach_stop_wait(&self) -> Option<DetachStopWait>
    {
        self.detach_stop_wait.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Sample the given tracee thread's Mach suspend count before the next detach
    /// sends its private SIGSTOP. The backend must still own the thread port. Tests only.
    pub fn observe_next_detach_suspend_count(&self, thread: ThreadId) -> mpsc::Receiver<ferros_core::error::Result<i32>>
    {
        let (sample, result) = mpsc::channel();
        *self.detach_suspend_probe.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(DetachSuspendProbe { thread, sample });
        result
    }

    pub(crate) fn sample_detach_suspend_count(&self)
    {
        let probe = self
            .detach_suspend_probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(probe) = probe {
            let _ = probe.sample.send(crate::diagnostics::thread_suspend_count(probe.thread));
        }
    }

    /// Deliver a real SIGUSR1 to another thread when the next private SIGSTOP arrives.
    /// `separate` processes that signal as an earlier, separate batch. Otherwise the
    /// two real kernel exceptions are processed in the same batch. Tests only.
    pub fn race_next_private_stop(&self, separate: bool)
    {
        self.private_stop_race.store(if separate { 1 } else { 2 }, Ordering::SeqCst);
    }

    /// Fail the next signal selection of a resume (`PT_THUPDATE` or the re-queued
    /// signal) before it reaches the kernel. Tests only (#250).
    pub fn fail_next_signal_selection(&self)
    {
        self.signal_selection.store(true, Ordering::SeqCst);
    }

    /// Make the exception loop handle the next Continue but never acknowledge it, as if
    /// the acknowledgement had not arrived within the reply timeout. Tests only (#250).
    pub fn drop_next_continue_ack(&self)
    {
        self.continue_ack.store(true, Ordering::SeqCst);
    }

    /// Make the next resume fail partway, after the task is running again: the backend
    /// takes one extra task suspend, its last `task_resume` resumes the task and then
    /// reports failure, and putting back the suspends already balanced fails. Tests only (#250).
    pub fn fail_next_task_resume_partway(&self)
    {
        self.resume_partway.store(true, Ordering::SeqCst);
    }

    /// Simulate an exited recipient when replaying the next handshake signal. Tests only.
    pub fn fail_next_handshake_signal_requeue(&self)
    {
        self.handshake_requeue.store(true, Ordering::SeqCst);
    }

    /// Whether the exception loop currently holds a reported stop. A signal deferred
    /// while the backend holds the task stopped (#79) does not count. Tests only.
    #[must_use]
    pub fn has_pending_stop(&self) -> bool
    {
        self.pending_stop.load(Ordering::SeqCst)
    }

    /// Pause the next nonprivate exception before publishing its stop. Tests only.
    pub fn pause_next_stop_publication(&self)
    {
        self.stop_publication.store(1, Ordering::SeqCst);
    }

    /// Whether the exception loop received the paused exception. Tests only.
    #[must_use]
    pub fn stop_publication_is_paused(&self) -> bool
    {
        self.stop_publication.load(Ordering::SeqCst) == 2
    }

    /// Release a real exception paused before publication. Tests only.
    pub fn release_stop_publication(&self)
    {
        let _ = self
            .stop_publication
            .compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Disarm the hook if no exception has reached it yet, or release the paused one.
    /// A test's drop guard calls this so an unwind leaves no armed hook to pause a
    /// late exception during teardown. Tests only (#183).
    pub fn cancel_stop_publication(&self)
    {
        if self
            .stop_publication
            .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.release_stop_publication();
        }
    }

    /// Whether the hook is armed and waiting for an exception to pause. Tests only.
    #[must_use]
    pub fn stop_publication_is_armed(&self) -> bool
    {
        self.stop_publication.load(Ordering::SeqCst) == 1
    }

    /// How long a paused publication may wait for release before the exception loop
    /// panics. Defaults to [`DEFAULT_STOP_PUBLICATION_DEADLINE`]. Tests only (#183).
    pub fn set_stop_publication_deadline(&self, deadline: std::time::Duration)
    {
        let millis = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX).max(1);
        self.stop_publication_deadline.store(millis, Ordering::SeqCst);
    }

    pub(super) fn stop_publication_deadline(&self) -> std::time::Duration
    {
        match self.stop_publication_deadline.load(Ordering::SeqCst) {
            0 => DEFAULT_STOP_PUBLICATION_DEADLINE,
            millis => std::time::Duration::from_millis(millis),
        }
    }

    /// Make the backend report `threads` sorted by thread port name, so Session steps
    /// breakpoint sites in a known order instead of `task_threads` order, which Mach
    /// does not promise across stops. Tests only (#183).
    pub fn sort_thread_lists(&self)
    {
        self.sorted_threads.store(true, Ordering::SeqCst);
    }

    /// Collect a real SIGUSR1 on `thread` alongside the next real SingleStep.
    /// The peer must have one backend-owned thread suspension and a SIGUSR1 handler.
    /// Its suspension is temporarily balanced to let the kernel deliver the signal,
    /// then restored before publishing the two events. No event is synthesized.
    pub fn batch_next_step_with_signal(&self, thread: ferros_core::ThreadId)
    {
        self.step_batch_signal
            .store(u32::try_from(thread.raw()).expect("thread port"), Ordering::SeqCst);
    }

    /// Publish that exception between the next resume's lock sections. Tests only.
    pub fn publish_paused_stop_during_next_resume(&self)
    {
        self.publish_during_resume.store(true, Ordering::SeqCst);
    }

    pub(crate) fn publish_resume_race(&self)
    {
        if self.publish_during_resume.swap(false, Ordering::SeqCst) {
            self.stop_publication.store(3, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !self.pending_stop.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline, "paused stop was not published");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// Let a competing debugger thread reap the next exited tracee before its watcher.
    /// Tests only; the watcher then observes a real `ECHILD`.
    pub fn reap_before_exit_watcher(&self)
    {
        self.other_exit_reaper.store(true, Ordering::SeqCst);
    }

    /// Pause the next birth step-off before the task is suspended. Tests only.
    ///
    /// The entry instruction is already restored and other threads are still
    /// scheduled, unless the backend suspended them for the step-off. The pause
    /// ends at [`Self::release_birth_step_off`] or after five seconds.
    pub fn hold_next_birth_step_off(&self)
    {
        self.birth_step_off_hold.store(1, Ordering::SeqCst);
    }

    /// Whether the exception loop is inside [`Self::hold_next_birth_step_off`].
    #[must_use]
    pub fn birth_step_off_is_held(&self) -> bool
    {
        self.birth_step_off_hold.load(Ordering::SeqCst) == 2
    }

    /// Let a held birth step-off suspend the task and restore the trap.
    pub fn release_birth_step_off(&self)
    {
        self.birth_step_off_hold.store(3, Ordering::SeqCst);
    }

    /// Make the next newborn `thread_suspend` fail. Tests only.
    pub fn fail_next_birth_suspend(&self)
    {
        self.birth_suspend.store(true, Ordering::SeqCst);
    }

    /// Make the exception loop's next `task_suspend`, for a new stop, fail before
    /// calling the kernel (`KERN_FAILURE`). The task keeps running. Tests only (#191).
    pub fn fail_next_exception_suspend(&self)
    {
        self.fail_exception_suspends(1);
    }

    /// Make the exception loop's next `count` `task_suspend`s for a new stop fail,
    /// retries included; 0 ends the injection. Tests only (#191).
    pub fn fail_exception_suspends(&self, count: u32)
    {
        self.exception_suspend.store(count, Ordering::SeqCst);
    }

    /// How long the exception loop retries a failed `task_suspend` before it gives up;
    /// defaults to [`SUSPEND_RETRY_BUDGET`]. Tests only (#198).
    pub fn set_suspend_retry_budget(&self, budget: std::time::Duration)
    {
        let millis = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX).max(1);
        self.suspend_retry_budget.store(millis, Ordering::SeqCst);
    }

    pub(super) fn suspend_retry_budget(&self) -> std::time::Duration
    {
        match self.suspend_retry_budget.load(Ordering::SeqCst) {
            0 => SUSPEND_RETRY_BUDGET,
            millis => std::time::Duration::from_millis(millis),
        }
    }

    /// How many `task_suspend`s the exception loop has attempted for a new stop so far,
    /// failed and injected ones included; an interrupt's handed-over suspend is not one.
    /// The loop counts them always; only tests read the count (#198).
    #[must_use]
    pub fn exception_suspend_attempts(&self) -> u32
    {
        self.exception_suspend_attempts.load(Ordering::SeqCst)
    }

    /// Make the next `count` `thread_suspend`s that hold a faulting thread after a
    /// failed `task_suspend` fail without calling the kernel, leaving that thread's
    /// exception unanswered; 0 ends the injection. Tests only (#198).
    pub fn fail_thread_freezes(&self, count: u32)
    {
        self.thread_freeze.store(count, Ordering::SeqCst);
    }

    /// Consume one injected thread-freeze failure.
    pub(super) fn take_thread_freeze_failure(&self) -> bool
    {
        self.thread_freeze
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
    }

    /// Consume one injected `task_suspend` failure.
    pub(super) fn take_exception_suspend_failure(&self) -> bool
    {
        if self.fail_until_detach.load(Ordering::SeqCst) {
            return true;
        }
        self.exception_suspend
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
    }

    /// Make the next `count` suppressions of a client signal before its exception
    /// reply (`PT_THUPDATE(0)`) fail without calling the kernel, leaving the exception
    /// unanswered; 0 ends the injection. Tests only (#191).
    pub fn fail_signal_suppressions(&self, count: u32)
    {
        self.signal_suppression.store(count, Ordering::SeqCst);
    }

    /// Pause the exception loop before its next retried `task_suspend` of a batch held
    /// behind a failed one, until a detach handshake has sent its private SIGSTOP; the
    /// detach then waits until the loop holds the shared lock for that suspend, before it
    /// balances the backend's suspends. Tests only (#191).
    pub fn hold_next_suspend_retry_for_detach(&self)
    {
        self.suspend_retry_hold.store(1, Ordering::SeqCst);
    }

    /// Whether the exception loop is paused by [`Self::hold_next_suspend_retry_for_detach`].
    #[must_use]
    pub fn suspend_retry_is_held(&self) -> bool
    {
        self.suspend_retry_hold.load(Ordering::SeqCst) == 2
    }

    /// Pause the exception loop before its next retried `task_suspend` of a batch held
    /// behind a failed one, and make the next detach send its private SIGSTOP late (#198,
    /// G1 and G3): only after it has armed its handshake and balanced the backend's
    /// suspends, then released the loop and waited until the loop has finished that
    /// retry under the shared lock. So the handshake arms after the loop checked for one
    /// at the top of its pass, and the private SIGSTOP's exception cannot arrive before
    /// the retry's suspend. The receiver gets whether the handshake still awaited its
    /// private stop when the detach sent it: `false` means an exception received earlier
    /// satisfied it. Tests only.
    pub fn send_next_detach_stop_after_suspend_retry(&self) -> mpsc::Receiver<bool>
    {
        let (probe, result) = mpsc::channel();
        *self.detach_stop_probe.lock().unwrap_or_else(PoisonError::into_inner) = Some(probe);
        self.detach_stop_hold.store(1, Ordering::SeqCst);
        result
    }

    /// Whether the exception loop is paused by
    /// [`Self::send_next_detach_stop_after_suspend_retry`].
    #[must_use]
    pub fn detach_stop_hold_is_paused(&self) -> bool
    {
        self.detach_stop_hold.load(Ordering::SeqCst) == 2
    }

    /// Whether this detach defers its private SIGSTOP for
    /// [`Self::send_next_detach_stop_after_suspend_retry`]: only while the loop is paused
    /// by it. A hook the loop never reached is disarmed, so it cannot pause a later retry.
    pub(crate) fn defer_detach_stop(&self) -> bool
    {
        if self
            .detach_stop_hold
            .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return false;
        }
        self.detach_stop_hold.load(Ordering::SeqCst) == 2
    }

    /// Release the retry paused by [`Self::send_next_detach_stop_after_suspend_retry`] and
    /// wait (bounded) until the loop holds the shared lock for it. The detach then takes
    /// that lock, so it sends its private SIGSTOP after the loop's pass.
    pub(crate) fn release_suspend_retry_before_detach_stop(&self)
    {
        if self
            .detach_stop_hold
            .compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.detach_stop_hold.load(Ordering::SeqCst) == 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.detach_stop_hold.store(0, Ordering::SeqCst);
    }

    /// Detach sends its private SIGSTOP now, under the shared lock: report to an armed
    /// probe whether the handshake still awaits its private stop.
    pub(crate) fn report_detach_stop_awaited(&self, awaited: bool)
    {
        let probe = self.detach_stop_probe.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(probe) = probe {
            let _ = probe.send(awaited);
        }
    }

    /// If `retry` and a test armed a hold, pause until a detach releases it (or five
    /// seconds pass).
    pub(super) fn pause_suspend_retry(&self, retry: bool)
    {
        if retry
            && self
                .detach_stop_hold
                .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.detach_stop_hold.load(Ordering::SeqCst) == 2 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // No detach released it in time: disarm, so a late detach does not wait.
            let _ = self
                .detach_stop_hold
                .compare_exchange(2, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
        if !retry
            || self
                .suspend_retry_hold
                .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.suspend_retry_hold.load(Ordering::SeqCst) == 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The loop holds the shared lock for the retry the hold paused.
    pub(super) fn suspend_retry_locked(&self)
    {
        let _ = self
            .suspend_retry_hold
            .compare_exchange(3, 0, Ordering::SeqCst, Ordering::SeqCst);
        let _ = self
            .detach_stop_hold
            .compare_exchange(3, 4, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Detach has sent its private SIGSTOP: release a paused suspend retry and wait
    /// (bounded) until the loop holds the shared lock for it.
    pub(crate) fn release_suspend_retry_for_detach(&self)
    {
        if self
            .suspend_retry_hold
            .compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.suspend_retry_hold.load(Ordering::SeqCst) == 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Make every exception-loop `task_suspend` for a new stop and every suppression of
    /// a client signal before its exception reply (`PT_THUPDATE(0)`) fail without calling
    /// the kernel, as [`Self::fail_exception_suspends`] and
    /// [`Self::fail_signal_suppressions`] do, until the next detach arms its private
    /// handshake; from then on both succeed. A client signal then stays held with its
    /// reply outstanding until the handshake begins. Tests only (#198).
    pub fn fail_suspends_and_suppressions_until_detach(&self)
    {
        self.fail_until_detach.store(true, Ordering::SeqCst);
    }

    /// Detach arms its handshake: end [`Self::fail_suspends_and_suppressions_until_detach`].
    /// Called under the shared lock, so nothing the loop does after the arming fails.
    pub(crate) fn detach_handshake_armed(&self)
    {
        self.fail_until_detach.store(false, Ordering::SeqCst);
    }

    /// At the next teardown of the exception handler, before the target's exception
    /// ports are restored, park the exception loop, make a running target thread raise a
    /// real single-step `EXC_BREAKPOINT`, and wait until it waits in that exception, its
    /// message queued on the port and unreceived; then shut the loop down (#198, F6). The
    /// loop must answer it before the port is destroyed: a destroyed exception message
    /// is an unhandled exception, and the kernel turns this one into a SIGTRAP that kills
    /// the released target. Needs a target thread that runs without waiting in the kernel,
    /// such as `hot_loop`'s. Tests only.
    pub fn queue_exception_before_next_teardown(&self)
    {
        self.teardown_exception.store(true, Ordering::SeqCst);
    }

    /// Whether [`Self::queue_exception_before_next_teardown`] queued its exception.
    #[must_use]
    pub fn exception_was_queued_before_teardown(&self) -> bool
    {
        self.teardown_exception_queued.load(Ordering::SeqCst)
    }

    /// The exception loop's pass starts here: park while a teardown hook asks for it
    /// (bounded, five seconds).
    pub(super) fn park_loop_for_teardown(&self)
    {
        if self
            .loop_park
            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.loop_park.load(Ordering::SeqCst) == 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.loop_park.store(0, Ordering::SeqCst);
    }

    /// The teardown half of [`Self::queue_exception_before_next_teardown`]: park the loop
    /// running on `exception_port`, then queue the exception from a running thread of
    /// `task`. Returns whether the loop is parked, for [`Self::unpark_loop`].
    #[cfg(target_os = "macos")]
    pub(crate) fn queue_teardown_exception(&self, task: mach_port_t, exception_port: mach_port_t) -> bool
    {
        if task == MACH_PORT_NULL || !self.teardown_exception.swap(false, Ordering::SeqCst) {
            return false;
        }
        self.loop_park.store(1, Ordering::SeqCst);
        wake_loop(exception_port);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.loop_park.load(Ordering::SeqCst) != 2 {
            if std::time::Instant::now() >= deadline {
                self.loop_park.store(0, Ordering::SeqCst);
                error!("teardown hook: the exception loop did not park");
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let queued = queue_single_step_exception(task, exception_port);
        self.teardown_exception_queued.store(queued, Ordering::SeqCst);
        true
    }

    /// End the park of [`Self::queue_teardown_exception`].
    pub(crate) fn unpark_loop(&self)
    {
        let _ = self.loop_park.compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Consume one injected signal-suppression failure.
    pub(super) fn take_signal_suppression_failure(&self) -> bool
    {
        if self.fail_until_detach.load(Ordering::SeqCst) {
            return true;
        }
        self.signal_suppression
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
    }

    pub(crate) fn fail_birth_suspend(&self) -> bool
    {
        self.birth_suspend.swap(false, Ordering::SeqCst)
    }

    /// If `relevant` and a test armed [`Self::hold_next_birth_step_off`], pause here.
    pub(crate) fn pause_for_birth_step_off(&self, relevant: bool)
    {
        if !relevant || self.birth_step_off_hold.load(Ordering::SeqCst) != 1 {
            return;
        }
        self.birth_step_off_hold.store(2, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.birth_step_off_hold.load(Ordering::SeqCst) == 2 {
            assert!(std::time::Instant::now() < deadline, "birth step-off hold was not released");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Consume a pending injected failure.
    pub(super) fn take(&self) -> bool
    {
        self.reply.swap(false, Ordering::SeqCst)
    }
}

/// How long [`queue_single_step_exception`] waits for one thread's exception message.
#[cfg(target_os = "macos")]
pub(super) const TEARDOWN_TRAP_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// How many threads [`queue_single_step_exception`] tries before it gives up.
#[cfg(target_os = "macos")]
pub(super) const TEARDOWN_TRAP_TRIES: usize = 8;

/// Messages queued on `port`, from `mach_port_get_attributes(MACH_PORT_RECEIVE_STATUS)`.
#[cfg(target_os = "macos")]
pub(super) fn queued_messages(port: mach_port_t) -> std::result::Result<u32, kern_return_t>
{
    // `mach_port_status_t`: ten natural_t words; `mps_msgcount` is the fifth.
    const MACH_PORT_RECEIVE_STATUS: c_int = 2;
    const MACH_PORT_RECEIVE_STATUS_COUNT: mach_msg_type_number_t = 10;
    const MPS_MSGCOUNT: usize = 4;
    let mut status = [0_u32; MACH_PORT_RECEIVE_STATUS_COUNT as usize];
    let mut count = MACH_PORT_RECEIVE_STATUS_COUNT;
    // SAFETY: `status` holds MACH_PORT_RECEIVE_STATUS_COUNT writable words and outlives
    // the call; `port` is a receive right this process holds.
    let kr = unsafe {
        crate::ffi::mach_port_get_attributes(
            mach_task_self(),
            port,
            MACH_PORT_RECEIVE_STATUS,
            status.as_mut_ptr(),
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(kr);
    }
    if (count as usize) <= MPS_MSGCOUNT {
        return Err(KERN_FAILURE);
    }
    Ok(status[MPS_MSGCOUNT])
}

/// Set or clear the single-step bit of a running `thread` under a balanced
/// `thread_suspend`.
#[cfg(target_os = "macos")]
pub(super) fn set_single_step_while_suspended(thread: thread_act_t, enabled: bool) -> bool
{
    // SAFETY: the caller holds a right to this tracee thread; the suspend is balanced
    // at once, after the single-step bit is written.
    unsafe {
        if crate::ffi::thread_suspend(thread) != KERN_SUCCESS {
            return false;
        }
        let set = crate::registers::set_single_step(thread, enabled).is_ok();
        crate::ffi::thread_resume(thread);
        set
    }
}

/// The test hook of [`ReplyFaultInjector::queue_exception_before_next_teardown`], with
/// the loop parked: make a running thread of `task` raise a single-step
/// `EXC_BREAKPOINT` and confirm that its message is queued, unreceived, on
/// `exception_port`. A thread that does not trap in time has its bit cleared and the
/// next running one is tried, from a fresh thread list. Returns whether a message was
/// queued; each failed step is logged.
#[cfg(target_os = "macos")]
pub(super) fn queue_single_step_exception(task: mach_port_t, exception_port: mach_port_t) -> bool
{
    // The parked loop receives nothing, and nothing else sends to the port meanwhile:
    // only a message queued after this baseline is the exception.
    let baseline = match queued_messages(exception_port) {
        Ok(count) => count,
        Err(kr) => {
            error!("teardown hook: cannot read the exception port's queue: {kr}");
            return false;
        }
    };
    let waiting = |thread: thread_act_t| crate::diagnostics::thread_is_waiting(ThreadId::from(u64::from(thread)));
    let mut tried: Vec<thread_act_t> = Vec::new();
    for attempt in 1..=TEARDOWN_TRAP_TRIES {
        let Ok(threads) = crate::threads::ThreadManager::refresh_thread_list(task, &[]) else {
            error!("teardown hook: cannot list the target's threads (attempt {attempt})");
            return false;
        };
        let candidate = threads
            .iter()
            .copied()
            .find(|thread| !tried.contains(thread) && matches!(waiting(*thread), Ok(false)));
        let queued = candidate.is_some_and(|thread| {
            tried.push(thread);
            if !set_single_step_while_suspended(thread, true) {
                warn!("teardown hook: cannot set the single-step bit of thread {thread} (attempt {attempt})");
                return false;
            }
            let deadline = std::time::Instant::now() + TEARDOWN_TRAP_WAIT;
            loop {
                match queued_messages(exception_port) {
                    Ok(count) if count > baseline => return true,
                    Ok(_) => {}
                    Err(kr) => {
                        error!("teardown hook: cannot read the exception port's queue: {kr}");
                        // Clear the bit, so the thread cannot trap after the teardown.
                        let _ = set_single_step_while_suspended(thread, false);
                        return false;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // It never trapped: clear the bit, so it cannot trap after the teardown,
            // unless its message arrived meanwhile.
            let cleared = set_single_step_while_suspended(thread, false);
            let late = queued_messages(exception_port).is_ok_and(|count| count > baseline);
            if !late {
                warn!(
                    "teardown hook: thread {thread} queued no exception within {TEARDOWN_TRAP_WAIT:?} (attempt {attempt}, \
                     bit cleared: {cleared})"
                );
            }
            late
        });
        if candidate.is_none() {
            warn!(
                "teardown hook: no running target thread left to try among {} (attempt {attempt})",
                threads.len()
            );
        }
        for thread in threads {
            deallocate_right(thread);
        }
        if queued {
            return true;
        }
        if candidate.is_none() {
            // Wait for a thread to run again before the next list.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    error!("teardown hook: no thread queued an exception in {TEARDOWN_TRAP_TRIES} attempts");
    false
}
