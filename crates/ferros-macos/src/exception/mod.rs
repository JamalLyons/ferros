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

use ferros_core::backend::{BackendError, BackendResult, FaultKind, StopEvent};
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

mod event;
mod fault_injector;
mod messages;
mod run;
mod stops;
mod suspend;

pub(crate) use event::{EXC_SOFT_SIGNAL, stop_event_from_exception, thread_state_flavor_for_arch};
pub use fault_injector::{DEFAULT_STOP_PUBLICATION_DEADLINE, ReplyFaultInjector};
#[allow(clippy::wildcard_imports)]
use messages::*;
pub(crate) use messages::{ExceptionLoopCommand, HeldException, RequeuedSignal, wake_loop};
#[cfg(target_os = "macos")]
pub(crate) use messages::{SavedExceptionPorts, get_exception_ports};
#[cfg(target_os = "macos")]
pub(crate) use run::{answer_held_exceptions, release_held_exceptions, run_exception_loop};
#[allow(clippy::wildcard_imports)]
use stops::*;
pub use suspend::SUSPEND_RETRY_BUDGET;
#[allow(clippy::wildcard_imports)]
use suspend::*;
