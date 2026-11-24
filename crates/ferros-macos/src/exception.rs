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
//! an exception port with `task_swap_exception_ports()` (keeping the target's previous
//! exception ports, which a detach puts back) and then receives exception messages via
//! `mach_msg()`.
//!
//! ## Stops
//!
//! When an exception arrives, the loop suspends the whole task (`task_suspend`)
//! before it reports anything, because the [`Backend`](ferros_core::Backend) contract
//! says every thread is stopped when `wait` returns an event. `task_suspend` returns once
//! the running threads have stopped, so a thread that raised an exception at the same
//! moment has queued its message by then: the loop drains the port without blocking and
//! reports one event per message. It counts one suspend per stop in
//! [`ExceptionSharedState::suspends`], answers client exception messages, and queues
//! their events under the shared lock. User code remains stopped by the task
//! suspension. Keeping replies unanswered would prevent external SIGKILL from
//! taking effect. The loop still services the port while stopped, suppressing and
//! answering newly arriving signals without resuming user code. Only the exit
//! watcher marks death and publishes terminal status.
//!
//! If that `task_suspend` fails (#191), nothing of the batch is published and no
//! Continue is awaited, but nothing is dropped either: every record of the batch,
//! including stops deferred behind an earlier hold, goes back to the front of the
//! deferred queue in order, so a single-step completion, a birth step-off or a
//! deferred signal is not lost. Each faulting thread is held with a `thread_suspend`
//! of its own before its exception is answered (a client signal is suppressed first,
//! as usual), so it cannot run on, and no unanswered reply holds back an external
//! SIGKILL: XNU does not complete a SIGKILL while a thread waits for its exception
//! reply. The failure is queued as an `Err` for the next `wait`, once per run of
//! failures. After a receive, which also collects new exceptions, the loop retries the
//! suspend; the first one that succeeds releases those thread suspends, the task
//! suspension now holding the threads, and publishes the batch through the usual birth
//! and handshake paths. A private attach/detach SIGSTOP is not retried: its exception
//! stays held for `PT_DETACH`, and the handshake caller receives the failure at once. A
//! handshake, a discard of deferred stops, shutdown and a dead task release the thread
//! suspends too.
//!
//! The retries back off (#198): the first receive lasts 10 ms (`SUSPEND_RETRY_MS`) and
//! each further consecutive failure doubles it, up to 500 ms (`SUSPEND_RETRY_MAX_MS`).
//! Once the failures have lasted [`SUSPEND_RETRY_BUDGET`] (5 s), the loop gives up: it
//! queues one last `Err` for `wait` saying the task cannot be suspended and the session
//! is unrecoverable, and calls `task_suspend` for client stops no more. The frozen
//! threads stay held, their exceptions answered, so an external SIGKILL still ends the
//! target; a new exception is held the same way, without another report. The loop
//! keeps servicing its port and commands every 500 ms, so detach, kill, a discard of
//! deferred stops, shutdown and task death release everything as before. An interrupt,
//! or a resume refused for the held batch, still hands a task suspend of its own over
//! and the loop publishes the batch with it: that is a real successful suspend, after
//! which the hold ends and a later failure starts a fresh budget. A detach handshake's
//! private SIGSTOP still gets a `task_suspend` of its own, so a detach can complete its
//! handshake if the task can be suspended again.
//!
//! A thread whose own `thread_suspend` fails is the limit of that hold (#198): its
//! exception stays unanswered, since answering it would let the thread run past a stop
//! that is not published, and XNU does not complete a SIGKILL until it is answered. The
//! loop tries that freeze again on each retry pass (and after the give-up), and the
//! first that succeeds answers the exception, so a SIGKILL sent meanwhile then takes
//! effect; detach, kill, shutdown and a dead task answer it too. A client signal is
//! answered only once it is also suppressed (`PT_THUPDATE`): if the freeze succeeds but
//! the suppression fails, its exception stays unanswered too, and so does a SIGKILL,
//! until a later pass suppresses it or teardown answers it.
//!
//! The backoff deadline of the next attempt does not move when a message arrives: an
//! exception raised during the wait is held at once, its thread frozen and its exception
//! answered as above, without a `task_suspend`, and the next attempt still waits for
//! the deadline. A wake (a command, shutdown, a handshake or an interrupt's handover)
//! ends the wait as before; a handover is published at once, and a handshake retries
//! after 10 ms. A failed `task_suspend` for a handshake's private SIGSTOP counts
//! against no retry budget: the handshake has taken the held batch apart.
//!
//! While such a batch is held, [`ExceptionSharedState::held_unsuspended`] is set. An
//! interrupt then takes the task suspend and hands it to the loop
//! ([`ExceptionSharedState::interrupt_suspend`]) instead of queueing `Interrupted`: the
//! loop publishes the held batch with that suspend as the stop, so a held step or
//! breakpoint cannot be published after a later resume it does not belong to. A thread
//! is held at most once per batch, and a failed signal suppression is retried with the
//! suspend but reported once. Only a fresh exception, whose reply is outstanding, can
//! be a handshake's private SIGSTOP: an answered client SIGSTOP of the batch is a
//! client signal. Shutdown joins the loop whenever it holds records, and the loop takes
//! commands in every state, so it answers what is queued before its port is destroyed.
//!
//! ## Held signals and retained thread rights
//!
//! Client Unix signals are suppressed with `PT_THUPDATE(0)` and answered while
//! the task remains suspended. Their logical stop records retain the thread right
//! and the client's delivery decision. On resume, a selected signal is requeued
//! with `__pthread_kill` on the same thread; a fresh kernel pass evaluates its
//! properties and the loop consumes its one internal stop. Suppression queues
//! nothing. Thus external SIGKILL can complete even before the client calls `wait`.
//! Signals arriving during an existing stop are also answered early, but their
//! logical events are deferred until resume. That includes the launch, attach and
//! interrupt stops, which the loop did not report itself: it then awaits Continue
//! as if it had (#79). Private attach/detach SIGSTOP replies
//! stay held for their ptrace handshake. During a handshake, competing application
//! signals are answered without creating client stops or adding task suspends, so
//! they cannot prevent the private SIGSTOP from arriving. Independent thread rights
//! preserve those signals for internal delivery after attach or detach completes.
//!
//! A selective resume uses `thread_suspend` to keep threads stopped. Their logical
//! exception records and signal decisions remain in
//! [`ExceptionSharedState::held`] until a later resume lets them run. Records own the
//! message's thread right even after an early reply; removing a record
//! releases that right exactly once.
//!
//! ## Single-step
//!
//! A thread that completes a hardware single-step raises `EXC_BREAKPOINT` with codes
//! `[EXC_ARM_BREAKPOINT, 0]`; a `brk` raises it with `[EXC_ARM_BREAKPOINT, pc]`, and no
//! trap can be at address 0. [`stop_event_from_exception`] reports the first as
//! `StopEvent::SingleStep` and the second as `StopEvent::Breakpoint`.
//!
//! ## Port rights
//!
//! Each exception message carries a send right for the faulting thread and one for its
//! task. The loop releases the task right when it processes the request (deferred
//! requests retain it until resume or shutdown), and the thread right when its stop record is released (the backend holds its own right to
//! the thread by then, so the name, which is the [`ThreadId`], stays valid). The reply
//! consumes the send-once reply right.
//!
//! ## Failed replies
//!
//! A reply that cannot be sent does not stop the loop: the failure is queued as an
//! `Err` on the event channel, so the next `wait` returns it, and the loop keeps
//! receiving.
//!
//! ## References
//!
//! - [task_set_exception_ports(3) man page](https://developer.apple.com/documentation/kernel/1538049-task_set_exception_ports/)
//! - [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418827-thread_set_state/)

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use ferros_core::backend::{BackendError, BackendResult, StopEvent};
use ferros_core::types::{Address, Architecture, ProcessId, ThreadId};
use libc::{c_int, mach_port_t, thread_act_t};
#[cfg(target_os = "macos")]
use mach2::exc::__Reply__exception_raise_t;
#[cfg(target_os = "macos")]
use mach2::exception_types::{
    EXC_ARITHMETIC, EXC_BAD_ACCESS, EXC_BAD_INSTRUCTION, EXC_BREAKPOINT, EXC_SOFTWARE, exception_behavior_t,
    exception_mask_t, exception_type_t,
};
#[cfg(target_os = "macos")]
use mach2::kern_return::{KERN_FAILURE, KERN_SUCCESS, kern_return_t};
#[cfg(target_os = "macos")]
use mach2::message::{
    MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE, MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS,
    MACH_MSGH_BITS_COMPLEX, MACH_RCV_INVALID_NAME, MACH_RCV_MSG, MACH_RCV_PORT_DIED, MACH_RCV_TIMEOUT,
    MACH_SEND_INVALID_DEST, MACH_SEND_MSG, MACH_SEND_TIMEOUT, mach_msg, mach_msg_body_t, mach_msg_destroy,
    mach_msg_header_t, mach_msg_port_descriptor_t, mach_msg_return_t, mach_msg_size_t, mach_msg_type_number_t,
};
#[cfg(target_os = "macos")]
use mach2::ndr::NDR_record;
#[cfg(target_os = "macos")]
use mach2::ndr::NDR_record_t;
#[cfg(target_os = "macos")]
use mach2::port::{MACH_PORT_DEAD, MACH_PORT_NULL};
#[cfg(target_os = "macos")]
use mach2::task::{
    task_get_exception_ports, task_resume, task_set_exception_ports, task_suspend, task_swap_exception_ports,
};
#[cfg(target_os = "macos")]
use mach2::thread_status::thread_state_flavor_t;
#[cfg(target_os = "macos")]
use mach2::traps::mach_task_self;
use tracing::{debug, error, warn};

/// An item on the stop event channel: a stop, or a failure the next `wait` reports.
pub(crate) type LoopEvent = BackendResult<StopEvent>;

/// State shared by the Mach exception loop and the backend, always changed under its lock.
#[derive(Debug, Default)]
pub(crate) struct ExceptionSharedState
{
    /// Number of `task_suspend` calls the backend holds on the task and has not yet
    /// balanced with `task_resume` (the launch or attach stop, exception stops, interrupts).
    pub suspends: u32,
    /// Resume temporarily owns the suspend count while preparing thread actions and
    /// signal decisions. Those suspends remain in force until Continue is acknowledged.
    pub preparing_resume: bool,
    /// Whether the loop has reported a stop, or deferred a signal that arrived while
    /// the backend held the task stopped, and awaits its Continue command, while still
    /// servicing the exception port. Kept records alone never set it.
    pub pending_exception: bool,
    /// Logical records for the current stop and threads a resume left suspended.
    /// Client exceptions are already replied to; private handshakes retain their reply.
    pub(crate) held: Vec<HeldException>,
    /// Threads and original signal numbers still awaiting a client decision. The stop records
    /// retain their thread rights until the threads are allowed to run.
    pub soft_signals: Vec<(thread_act_t, c_int)>,
    /// Selected signals queued for a fresh kernel pass, replied to once internally.
    pub requeued_signals: Vec<RequeuedSignal>,
    /// An internal SIGSTOP handshake for attach/detach, never exposed as an event. It
    /// receives `Ok` once the private stop is held, or the error that prevented it.
    pub internal_stop: Option<mpsc::Sender<BackendResult<()>>>,
    /// Signals caught during a private attach/detach handshake. They are delivered
    /// internally after the private stop, never exposed as stale client stop events.
    pub handshake_signals: Option<Vec<RequeuedSignal>>,
    /// Set by the exit watcher before publishing the terminal event.
    pub dead: bool,
    /// Sticky watcher failure: no later terminal event can arrive from that watcher.
    pub watcher_error: Option<BackendError>,
    /// Traps that hold threads created while the default resume action is `Suspend`.
    pub(crate) birth: crate::birth::BirthPolicy,
    /// The loop holds a batch it could not publish because its `task_suspend` failed,
    /// and retries that suspend (#191). Set and cleared by the loop only.
    pub held_unsuspended: bool,
    /// An interrupt found [`Self::held_unsuspended`] set, took a task suspend of its own
    /// (counted in [`Self::suspends`]) and handed it to the loop, which publishes the
    /// held batch with it as the stop. A resume refused for a held batch hands over one
    /// of the suspends the backend already holds the same way. Cleared by the loop when
    /// it takes it over.
    pub interrupt_suspend: bool,
    /// Incremented each time attach or detach arms a private handshake. The loop stamps
    /// every exception it receives with the value current at that moment, so only one
    /// received after the handshake armed can be its private SIGSTOP (#191).
    pub handshake_generation: u64,
}

impl ExceptionSharedState
{
    pub(crate) fn new() -> Self
    {
        Self::default()
    }

    /// Arm a private attach/detach handshake: `ready` receives the outcome of its
    /// SIGSTOP. Exceptions received before this call are client exceptions.
    pub(crate) fn arm_handshake(&mut self, ready: mpsc::Sender<BackendResult<()>>)
    {
        self.handshake_generation += 1;
        self.internal_stop = Some(ready);
        self.handshake_signals = Some(Vec::new());
    }

    /// Private handshake signals (or failed early suppressions) still need PT_THUPDATE.
    pub(crate) fn has_signal_reply(&self, thread: thread_act_t) -> bool
    {
        self.held
            .iter()
            .any(|stop| stop.thread == thread && stop.soft_signal && stop.header.is_some())
    }
}

/// Lock the shared exception state, ignoring poisoning (the state stays consistent).
pub(crate) fn lock_shared(shared: &Mutex<ExceptionSharedState>) -> std::sync::MutexGuard<'_, ExceptionSharedState>
{
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    reply: Arc<AtomicBool>,
    pub(crate) resume: Arc<AtomicBool>,
    restore: Arc<AtomicBool>,
    private_stop_race: Arc<AtomicI32>,
    pending_stop: Arc<AtomicBool>,
    /// 0 idle, 1 armed, 2 paused in the exception loop, 3 released by the test.
    stop_publication: Arc<AtomicU8>,
    /// How long a paused publication waits for release before the loop panics, in
    /// milliseconds; 0 means [`DEFAULT_STOP_PUBLICATION_DEADLINE`]. Tests only.
    stop_publication_deadline: Arc<AtomicU64>,
    /// Report `threads` sorted by thread port name, so a test can rely on the order
    /// Session steps breakpoint sites in. Tests only (#183).
    pub(crate) sorted_threads: Arc<AtomicBool>,
    publish_during_resume: Arc<AtomicBool>,
    step_batch_signal: Arc<std::sync::atomic::AtomicU32>,
    pub(crate) other_exit_reaper: Arc<AtomicBool>,
    pub(crate) handshake_requeue: Arc<AtomicBool>,
    pub(crate) detach_handshake: Arc<AtomicBool>,
    /// Detach's private stop: 0 normal, 1 SIGCONT before the reply, 2 never observe
    /// the stop. Tests only (#149).
    pub(crate) detach_stop_wait: Arc<AtomicU8>,
    detach_suspend_probe: Arc<Mutex<Option<DetachSuspendProbe>>>,
    /// 0 idle, 1 armed, 2 paused in the exception loop, 3 released by the test.
    birth_step_off_hold: Arc<AtomicU8>,
    /// Fail the next `thread_suspend` of a newborn. Tests only.
    birth_suspend: Arc<AtomicBool>,
    /// How many of the exception loop's next `task_suspend`s for a new stop fail. Tests only.
    exception_suspend: Arc<std::sync::atomic::AtomicU32>,
    /// How many of the next client-signal suppressions (`PT_THUPDATE(0)`) fail. Tests only.
    signal_suppression: Arc<std::sync::atomic::AtomicU32>,
    /// 0 idle, 1 armed, 2 paused before a suspend retry, 3 released by a detach
    /// handshake. Tests only (#191).
    suspend_retry_hold: Arc<AtomicU8>,
    /// The retry budget of a failed exception `task_suspend` in milliseconds; 0 means
    /// [`SUSPEND_RETRY_BUDGET`]. Tests only (#198).
    suspend_retry_budget: Arc<AtomicU64>,
    /// How many `task_suspend`s the exception loop has attempted for a new stop,
    /// injected failures included. Counted always, read by tests only (#198).
    exception_suspend_attempts: Arc<std::sync::atomic::AtomicU32>,
    /// How many of the next [`freeze_thread`]s fail without calling the kernel. Tests only (#198).
    thread_freeze: Arc<std::sync::atomic::AtomicU32>,
    /// 0 idle, 1 armed, 2 paused before a suspend retry, 3 released by a detach that
    /// armed its handshake, 4 the loop holds the shared lock for that retry. Tests only (#198).
    detach_stop_hold: Arc<AtomicU8>,
    /// Receives whether the handshake still awaited its private stop when the detach
    /// held by [`Self::send_next_detach_stop_after_suspend_retry`] sent it. Tests only (#198).
    detach_stop_probe: Arc<Mutex<Option<mpsc::Sender<bool>>>>,
    /// Every exception `task_suspend` for a new stop and every client-signal suppression
    /// fail until the next detach arms its handshake. Tests only (#198).
    fail_until_detach: Arc<AtomicBool>,
    /// Queue a real exception on the port at the next teardown. Tests only (#198, F6).
    teardown_exception: Arc<AtomicBool>,
    /// Set once that exception is queued on the port, unanswered. Tests only (#198).
    teardown_exception_queued: Arc<AtomicBool>,
    /// 0 idle, 1 requested by a teardown, 2 parked at the top of a loop pass, 3 released.
    /// Tests only (#198).
    loop_park: Arc<AtomicU8>,
}

#[derive(Debug)]
struct DetachSuspendProbe
{
    thread: ThreadId,
    sample: mpsc::Sender<ferros_core::error::Result<i32>>,
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
    /// it, as for a target not scheduled for over a second. Tests only (#149).
    pub fn expire_next_detach_stop_wait(&self)
    {
        self.detach_stop_wait.store(1, Ordering::SeqCst);
    }

    /// Make the next detach never observe its private SIGSTOP taking effect, so it
    /// must report that the released process may stay stopped. Tests only (#149).
    pub fn hide_next_detach_stop(&self)
    {
        self.detach_stop_wait.store(2, Ordering::SeqCst);
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

    fn stop_publication_deadline(&self) -> std::time::Duration
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

    fn suspend_retry_budget(&self) -> std::time::Duration
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
    fn take_thread_freeze_failure(&self) -> bool
    {
        self.thread_freeze
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
    }

    /// Consume one injected `task_suspend` failure.
    fn take_exception_suspend_failure(&self) -> bool
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
    fn pause_suspend_retry(&self, retry: bool)
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
    fn suspend_retry_locked(&self)
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
    fn park_loop_for_teardown(&self)
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
    fn take_signal_suppression_failure(&self) -> bool
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
    fn take(&self) -> bool
    {
        self.reply.swap(false, Ordering::SeqCst)
    }
}

/// How long [`queue_single_step_exception`] waits for one thread's exception message.
#[cfg(target_os = "macos")]
const TEARDOWN_TRAP_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// How many threads [`queue_single_step_exception`] tries before it gives up.
#[cfg(target_os = "macos")]
const TEARDOWN_TRAP_TRIES: usize = 8;

/// Messages queued on `port`, from `mach_port_get_attributes(MACH_PORT_RECEIVE_STATUS)`.
#[cfg(target_os = "macos")]
fn queued_messages(port: mach_port_t) -> std::result::Result<u32, kern_return_t>
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
fn set_single_step_while_suspended(thread: thread_act_t, enabled: bool) -> bool
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
fn queue_single_step_exception(task: mach_port_t, exception_port: mach_port_t) -> bool
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

/// MIG id of `mach_exception_raise`, the request the kernel sends for
/// `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES`.
#[cfg(target_os = "macos")]
const MACH_EXCEPTION_RAISE_ID: i32 = 2405;

/// Id of the empty message [`wake_loop`] sends to the exception port. It is outside the
/// MIG `mach_exc` subsystem (2405 to 2407), so it is never taken for a request.
#[cfg(target_os = "macos")]
const WAKE_LOOP_ID: i32 = 0x6665_7272;

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

/// A logical exception stop, retaining its thread right and any unanswered reply.
#[derive(Debug)]
pub(crate) struct HeldException
{
    /// An unanswered request's send-once reply right; None after it is consumed.
    header: Option<mach_msg_header_t>,
    /// The message's thread send right, retained until its logical stop is released.
    thread: mach_port_t,
    soft_signal: bool,
    /// [`ExceptionSharedState::handshake_generation`] when the loop received it.
    handshake: u64,
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
const EXCEPTION_PORT_SLOTS: usize = 32;

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
struct ExceptionPortArrays
{
    masks: [exception_mask_t; EXCEPTION_PORT_SLOTS],
    handlers: [mach_port_t; EXCEPTION_PORT_SLOTS],
    behaviors: [exception_behavior_t; EXCEPTION_PORT_SLOTS],
    flavors: [thread_state_flavor_t; EXCEPTION_PORT_SLOTS],
    count: mach_msg_type_number_t,
}

#[cfg(target_os = "macos")]
impl ExceptionPortArrays
{
    fn new() -> Self
    {
        Self {
            masks: [0; EXCEPTION_PORT_SLOTS],
            handlers: [MACH_PORT_NULL; EXCEPTION_PORT_SLOTS],
            behaviors: [0; EXCEPTION_PORT_SLOTS],
            flavors: [0; EXCEPTION_PORT_SLOTS],
            count: EXCEPTION_PORT_SLOTS as mach_msg_type_number_t,
        }
    }

    fn entries(&self) -> Vec<ExceptionPortEntry>
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
    entries: Vec<ExceptionPortEntry>,
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
/// - `EXC_BREAKPOINT` with codes `[EXC_ARM_BREAKPOINT, 0]` -> `StopEvent::SingleStep`:
///   XNU's `handle_sw_step_debug` (`osfmk/arm64/sleh.c`) raises exactly these codes when
///   a hardware single-step completes. (On x86-64, `EXC_I386_SGL` is also 1.)
/// - any other `EXC_BREAKPOINT` -> `StopEvent::Breakpoint` with the raw `pc` (not rewound;
///   on arm64 it already points at the `brk`, whose codes are `[EXC_ARM_BREAKPOINT, pc]`,
///   and the engine maps it with `Arch::pc_after_trap`)
/// - `EXC_BAD_ACCESS` -> `StopEvent::Signal(SIGSEGV)`
/// - `EXC_BAD_INSTRUCTION` -> `StopEvent::Signal(SIGILL)`
/// - `EXC_ARITHMETIC` -> `StopEvent::Signal(SIGFPE)`
/// - `EXC_SOFTWARE/EXC_SOFT_SIGNAL` -> the held Unix signal; other software exceptions -> SIGTRAP
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
        EXC_BREAKPOINT if codes == [EXC_ARM_BREAKPOINT, 0] => StopEvent::SingleStep { thread },
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
        EXC_SOFTWARE if codes[0] == EXC_SOFT_SIGNAL => signal(codes[1] as c_int),
        EXC_SOFTWARE => signal(libc::SIGTRAP),
        other => StopEvent::Other {
            thread: Some(thread),
            reason: format!("Mach exception {other}, codes {codes:?}"),
        },
    }
}

/// Unix signal exception code from `<mach/exception_types.h>`.
pub(crate) const EXC_SOFT_SIGNAL: i64 = 0x10003;

/// `EXC_ARM_BREAKPOINT` from `<mach/arm/exception.h>`: the code of every arm64
/// `EXC_BREAKPOINT` (a `brk`, a hardware breakpoint, or a completed single-step).
pub(crate) const EXC_ARM_BREAKPOINT: i64 = 1;

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

/// How long the loop receives before it retries a failed `task_suspend` the first
/// time (#191). Each further consecutive failure doubles it, up to
/// [`SUSPEND_RETRY_MAX_MS`] (#198).
#[cfg(target_os = "macos")]
const SUSPEND_RETRY_MS: u32 = 10;

/// The longest delay between two retries of a failed `task_suspend`, and how long the
/// loop receives between passes once it has given up (#198).
#[cfg(target_os = "macos")]
const SUSPEND_RETRY_MAX_MS: u32 = 500;

/// How long consecutive `task_suspend` failures may last before the loop gives up
/// retrying and reports the session unrecoverable (#198). A test can shorten it with
/// [`ReplyFaultInjector::set_suspend_retry_budget`].
pub const SUSPEND_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The retry state of a batch held behind failed `task_suspend`s (#191, #198).
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct SuspendRetry
{
    /// Consecutive failed attempts; 0 while no batch is held.
    failures: u32,
    /// When the first of them failed.
    since: Option<std::time::Instant>,
    /// The last failure's code, reported for a batch held after the give-up.
    last_kr: kern_return_t,
    /// When the next attempt is due; messages that arrive earlier do not advance it.
    next_at: Option<std::time::Instant>,
    /// The failures outlasted the budget: the loop no longer calls `task_suspend`.
    gave_up: bool,
}

#[cfg(target_os = "macos")]
impl SuspendRetry
{
    fn failed(&mut self, kr: kern_return_t)
    {
        self.failures = self.failures.saturating_add(1);
        self.since.get_or_insert_with(std::time::Instant::now);
        self.last_kr = kr;
    }

    /// The delay before the next retry: [`SUSPEND_RETRY_MS`] doubled per failure after
    /// the first, at most [`SUSPEND_RETRY_MAX_MS`], which is also the pace after the
    /// give-up.
    fn delay_ms(&self) -> u32
    {
        if self.gave_up {
            return SUSPEND_RETRY_MAX_MS;
        }
        // Six doublings already pass the ceiling; the bound keeps the shift in range.
        let doublings = self.failures.saturating_sub(1).min(6);
        (SUSPEND_RETRY_MS << doublings).min(SUSPEND_RETRY_MAX_MS)
    }

    /// Set the next attempt one [`Self::delay_ms`] from now.
    fn schedule(&mut self)
    {
        self.next_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(u64::from(self.delay_ms())));
    }

    /// Whether the next attempt is due.
    fn due(&self) -> bool
    {
        self.next_at.is_none_or(|next| std::time::Instant::now() >= next)
    }

    /// Milliseconds until the next attempt is due, rounded up; 0 once it is.
    fn remaining_ms(&self) -> u32
    {
        self.next_at.map_or(0, |next| {
            let left = next.saturating_duration_since(std::time::Instant::now());
            u32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(SUSPEND_RETRY_MAX_MS)
        })
    }

    fn elapsed(&self) -> std::time::Duration
    {
        self.since.map_or(std::time::Duration::ZERO, |since| since.elapsed())
    }

    fn exhausted(&self, budget: std::time::Duration) -> bool
    {
        self.elapsed() >= budget
    }
}

/// How long the loop receives, with no stop pending, while it keeps records for
/// threads a resume left suspended, before it checks its commands again.
#[cfg(target_os = "macos")]
const KEPT_RECORDS_POLL_MS: u32 = 100;

/// The loop no longer holds a batch behind a failed `task_suspend` (#191), and has
/// published none of it. An interrupt that took the task suspend for that batch gets
/// its own stop instead, as `interrupt` would have reported it.
#[cfg(target_os = "macos")]
fn end_unsuspended_hold(state: &mut ExceptionSharedState, events: &mpsc::Sender<LoopEvent>)
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
fn hold_unsuspended_stops(
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
fn freeze_thread(thread: thread_act_t) -> bool
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
fn thaw_threads(frozen: &mut Vec<thread_act_t>)
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

/// Receive one message from `exception_port`: block until one arrives, or with
/// `poll`, return `Err(MACH_RCV_TIMED_OUT)` at once when none is queued.
#[cfg(target_os = "macos")]
fn receive_exception(exception_port: mach_port_t, poll: bool) -> std::result::Result<ReceiveBuffer, mach_msg_return_t>
{
    receive_exception_with_timeout(exception_port, poll.then_some(0))
}

/// Receive the next message other than a [`wake_loop`] wake, for the fault-injection
/// hooks that expect a real exception request next: a wake left queued by a `Continue`
/// the loop took before the wake arrived is skipped.
#[cfg(target_os = "macos")]
fn receive_request_for_hook(exception_port: mach_port_t) -> std::result::Result<ReceiveBuffer, mach_msg_return_t>
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
fn receive_exception_with_timeout(
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
fn decode_request(buffer: &ReceiveBuffer) -> Option<MachExceptionRaiseRequest>
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

/// Release threads suspended so they could not pass a disarmed birth site.
fn thaw_birth_freeze(state: &mut ExceptionSharedState) -> std::result::Result<(), String>
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
enum BirthConsume
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
fn try_consume_birth(
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
fn continue_running_thread(
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
        let _ = events.send(Err(BackendError::Os {
            code: -1,
            message: format!("thread {} could not pass its pthread entry: {err}", exception.thread),
        }));
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
fn consume_image_notifier(
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
fn suspend_newborn(
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
fn hold_exception(
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
fn prepare_stop(exception: &mut HeldException, pid: libc::pid_t, faults: &ReplyFaultInjector) -> BackendResult<()>
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
fn preserve_handshake_stop(
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
fn held_quietly(state: &ExceptionSharedState) -> bool
{
    state.suspends > 0
        && !state.preparing_resume
        && !state.pending_exception
        && state.handshake_signals.is_none()
        && !state.interrupt_suspend
}

/// The current [`ExceptionSharedState::handshake_generation`].
fn current_generation(shared: &Mutex<ExceptionSharedState>) -> u64
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
fn is_internal_stop(exception: &HeldException, event: &StopEvent, state: &ExceptionSharedState) -> bool
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
fn reply_to_requeued_signal(
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
fn reply_to_held(exception: &mut HeldException, faults: &ReplyFaultInjector) -> BackendResult<()>
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

/// Release one send right of this task, ignoring `MACH_PORT_NULL` and `MACH_PORT_DEAD`.
#[cfg(target_os = "macos")]
fn deallocate_right(name: mach_port_t)
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
fn reply_to_queued_exceptions(exception_port: mach_port_t)
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
fn reply_to_request(request: &MachExceptionRaiseRequest)
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
fn send_exception_reply(request: &mach_msg_header_t) -> std::result::Result<(), kern_return_t>
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
    use super::*;

    #[test]
    fn hardware_fault_is_not_requeued_as_a_unix_signal_during_a_handshake()
    {
        // A hardware exception may map to Signal for the portable event API, but
        // only EXC_SOFT_SIGNAL denotes a Unix signal that can be replayed.
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
            &StopEvent::Signal {
                thread: ThreadId::from(u64::from(thread)),
                signal: libc::SIGSEGV,
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

    const THREAD: u64 = 0x1403;

    fn event(exception: u32, pc: Option<u64>, codes: [i64; 2]) -> StopEvent
    {
        stop_event_from_exception(exception as exception_type_t, ThreadId::from(THREAD), pc, codes)
    }

    #[test]
    fn completed_single_step_is_a_single_step_stop()
    {
        // What `handle_sw_step_debug` raises; the pc is irrelevant.
        assert_eq!(
            event(EXC_BREAKPOINT, Some(0x1_0000_0aac), [EXC_ARM_BREAKPOINT, 0]),
            StopEvent::SingleStep {
                thread: ThreadId::from(THREAD)
            }
        );
        assert_eq!(
            event(EXC_BREAKPOINT, None, [EXC_ARM_BREAKPOINT, 0]),
            StopEvent::SingleStep {
                thread: ThreadId::from(THREAD)
            }
        );
    }

    #[test]
    fn brk_is_a_breakpoint_stop_at_the_raw_pc()
    {
        // A `brk` carries its own address as the second code.
        assert_eq!(
            event(EXC_BREAKPOINT, Some(0x1_0000_0aa8), [EXC_ARM_BREAKPOINT, 0x1_0000_0aa8]),
            StopEvent::Breakpoint {
                thread: ThreadId::from(THREAD),
                pc: Address::from(0x1_0000_0aa8),
            }
        );
        // Any other breakpoint code is a breakpoint too, never a step.
        assert!(matches!(
            event(EXC_BREAKPOINT, Some(0x40), [2, 0]),
            StopEvent::Breakpoint { .. }
        ));
        assert!(matches!(
            event(EXC_BREAKPOINT, None, [EXC_ARM_BREAKPOINT, 0x40]),
            StopEvent::Other { .. }
        ));
    }

    #[test]
    fn other_exceptions_keep_their_mapping()
    {
        let thread = ThreadId::from(THREAD);
        assert_eq!(
            event(EXC_BAD_ACCESS, None, [1, 0]),
            StopEvent::Signal {
                thread,
                signal: libc::SIGSEGV
            }
        );
        assert_eq!(
            event(EXC_SOFTWARE, None, [EXC_SOFT_SIGNAL, i64::from(libc::SIGUSR1)]),
            StopEvent::Signal {
                thread,
                signal: libc::SIGUSR1
            }
        );
    }
}
