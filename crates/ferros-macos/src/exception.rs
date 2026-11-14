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
//! ## Held signals and retained thread rights
//!
//! Client Unix signals are suppressed with `PT_THUPDATE(0)` and answered while
//! the task remains suspended. Their logical stop records retain the thread right
//! and the client's delivery decision. On resume, a selected signal is requeued
//! with `__pthread_kill` on the same thread; a fresh kernel pass evaluates its
//! properties and the loop consumes its one internal stop. Suppression queues
//! nothing. Thus external SIGKILL can complete even before the client calls `wait`.
//! Signals arriving during an existing stop are also answered early, but their
//! logical events are deferred until resume. Private attach/detach SIGSTOP replies
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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
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
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
#[cfg(target_os = "macos")]
use mach2::message::{
    MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE, MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX,
    MACH_RCV_INVALID_NAME, MACH_RCV_MSG, MACH_RCV_PORT_DIED, MACH_RCV_TIMEOUT, MACH_SEND_INVALID_DEST, MACH_SEND_MSG,
    mach_msg, mach_msg_body_t, mach_msg_destroy, mach_msg_header_t, mach_msg_port_descriptor_t, mach_msg_return_t,
    mach_msg_size_t, mach_msg_type_number_t,
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
    /// Whether the loop has reported a stop and awaits its Continue command, while
    /// still servicing the exception port. Kept records alone never set it.
    pub pending_exception: bool,
    /// Logical records for the current stop and threads a resume left suspended.
    /// Client exceptions are already replied to; private handshakes retain their reply.
    pub(crate) held: Vec<HeldException>,
    /// Threads and original signal numbers still awaiting a client decision. The stop records
    /// retain their thread rights until the threads are allowed to run.
    pub soft_signals: Vec<(thread_act_t, c_int)>,
    /// Selected signals queued for a fresh kernel pass, replied to once internally.
    pub requeued_signals: Vec<RequeuedSignal>,
    /// An internal SIGSTOP handshake for attach/detach, never exposed as an event.
    pub internal_stop: Option<mpsc::Sender<()>>,
    /// Signals caught during a private attach/detach handshake. They are delivered
    /// internally after the private stop, never exposed as stale client stop events.
    pub handshake_signals: Option<Vec<RequeuedSignal>>,
    /// Set by the exit watcher before publishing the terminal event.
    pub dead: bool,
    /// Sticky watcher failure: no later terminal event can arrive from that watcher.
    pub watcher_error: Option<BackendError>,
    /// Traps that hold threads created while the default resume action is `Suspend`.
    pub(crate) birth: crate::birth::BirthPolicy,
}

impl ExceptionSharedState
{
    pub(crate) fn new() -> Self
    {
        Self::default()
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
    stop_publication: Arc<AtomicU8>,
    publish_during_resume: Arc<AtomicBool>,
    pub(crate) other_exit_reaper: Arc<AtomicBool>,
    pub(crate) handshake_requeue: Arc<AtomicBool>,
    /// 0 idle, 1 armed, 2 paused in the exception loop, 3 released by the test.
    birth_step_off_hold: Arc<AtomicU8>,
    /// Fail the next `thread_suspend` of a newborn. Tests only.
    birth_suspend: Arc<AtomicBool>,
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

    /// Whether the exception loop currently holds a reported stop. Tests only.
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

/// MIG id of `mach_exception_raise`, the request the kernel sends for
/// `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES`.
#[cfg(target_os = "macos")]
const MACH_EXCEPTION_RAISE_ID: i32 = 2405;

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
/// Continue releases records of runnable threads and answers their remaining replies;
/// Shutdown answers all held replies, drains queued messages and exits. Destroying
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
    loop {
        let pending = lock_shared(&shared).pending_exception;
        if pending {
            match commands.try_recv() {
                Ok(ExceptionLoopCommand::Continue {
                    ack,
                    keep,
                    discard_deferred,
                }) => {
                    if discard_deferred {
                        for (mut exception, _) in deferred.drain(..) {
                            let _ = reply_to_held(&mut exception, &faults);
                            deallocate_right(exception.thread);
                        }
                    }
                    let mut state = lock_shared(&shared);
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
        }
        let mut first = if !pending && !deferred.is_empty() {
            deferred.pop_front().expect("deferred request")
        } else {
            let buffer = match receive_exception_with_timeout(exception_port, pending.then_some(10)) {
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
            if reply_to_requeued_signal(&request, &mut lock_shared(&shared), pid, &events, &faults) {
                continue;
            }
            hold_exception(&request, architecture)
        };
        let mut injected_batch = Vec::new();
        if is_internal_stop(&first.1, &lock_shared(&shared)) {
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
                let buffer = receive_exception_with_timeout(exception_port, Some(1000)).expect("racing signal exception");
                let request = decode_request(&buffer).expect("racing exception request");
                let mut racing = hold_exception(&request, architecture);
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
                    let buffer = receive_exception_with_timeout(exception_port, Some(1000)).expect("fresh private stop");
                    let request = decode_request(&buffer).expect("private exception request");
                    first = hold_exception(&request, architecture);
                    injected_batch.push(racing);
                }
            }
        }
        {
            let mut state = lock_shared(&shared);
            if state.handshake_signals.is_some() && !is_internal_stop(&first.1, &state) {
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
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while faults.stop_publication.load(Ordering::SeqCst) == 2 {
                assert!(std::time::Instant::now() < deadline, "paused stop was not released");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            faults.stop_publication.store(0, Ordering::SeqCst);
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
                && !is_internal_stop(&event, &state)
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

            // Stop every thread, collect every message of this stop, mark the exceptions
            // as held and queue the events in one step under the lock, so a concurrent
            // resume or interrupt sees either none or all of this stop.
            let mut state = lock_shared(&shared);
            // SAFETY: `task` is the send right the backend holds for as long as this
            // loop runs.
            let kr = unsafe { task_suspend(task) };
            if kr == KERN_SUCCESS {
                state.suspends += 1;
            } else {
                error!("task_suspend failed while handling a Mach exception: {kr}");
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
                    stops.push(hold_exception(&request, architecture));
                }
            }
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
                let internal = is_internal_stop(&event, &state);
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
                    let _ = ready.send(());
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
    for (mut exception, _) in deferred {
        let _ = reply_to_held(&mut exception, &faults);
        deallocate_right(exception.thread);
    }
    // The port was destroyed while the loop was blocked receiving: exceptions kept for
    // suspended threads are still held. Answer them, so those threads are not left
    // blocked in the kernel, and release their rights.
    release_held_exceptions(&mut lock_shared(&shared));
}

/// Receive one message from `exception_port`: block until one arrives, or with
/// `poll`, return `Err(MACH_RCV_TIMED_OUT)` at once when none is queued.
#[cfg(target_os = "macos")]
fn receive_exception(exception_port: mach_port_t, poll: bool) -> std::result::Result<ReceiveBuffer, mach_msg_return_t>
{
    receive_exception_with_timeout(exception_port, poll.then_some(0))
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
fn hold_exception(request: &MachExceptionRaiseRequest, architecture: Architecture) -> (HeldException, StopEvent)
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

fn is_internal_stop(event: &StopEvent, state: &ExceptionSharedState) -> bool
{
    matches!(
        event,
        StopEvent::Signal {
            signal: libc::SIGSTOP,
            ..
        }
    ) && state.internal_stop.is_some()
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
