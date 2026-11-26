//! Resumable, serial software-breakpoint step-overs.

use std::collections::VecDeque;

use super::exits::{ExitHooks, ExitStep};
use super::lifecycle::SignalPolicy;
use super::{Session, SessionState};
use crate::backend::{BackendError, BackendResult, ResumeRequest, StopEvent, ThreadAction};
use crate::breakpoints::{BreakpointEntry, BreakpointKind};
use crate::error::{DebuggerError, Result};
use crate::types::{Address, ThreadId};

#[derive(Clone)]
pub(super) struct StepSite
{
    thread: ThreadId,
    entry: BreakpointEntry,
}

/// A step-resume whose signal handler stopped before returning (another breakpoint, an
/// interrupt): the handler's later return to `address` with `sp` is the rendezvous, not a
/// user hit.
pub(super) struct HandlerReturn
{
    thread: ThreadId,
    address: Address,
    sp: Address,
    /// The user's continue since the handler stopped, reissued after stepping over the
    /// original instruction. The signal policy applies to whatever is still held then.
    request: Option<(ResumeRequest, SignalPolicy)>,
}

impl HandlerReturn
{
    pub(super) fn address(&self) -> Address
    {
        self.address
    }
}

/// The next move after a stop of a pending handler return's exit traps.
enum Follow
{
    /// Single-step the handler's thread through the exit.
    Step,
    /// Reissue the user's continue.
    Continue,
    /// Report this stop.
    Report(StopEvent),
}

#[derive(Clone)]
pub(super) struct StepPlan
{
    current: StepSite,
    remaining: VecDeque<StepSite>,
    // None means the user requested a single step: report its completion.
    final_request: Option<ResumeRequest>,
    signal_return_sp: Option<Address>,
}

/// How long `refused_wait` waits for the stop a resume was refused for. The backend has
/// it queued already, so this bounds only the case where the target died meanwhile and
/// its terminal event is still being collected. Inside [`Session::wait`] the caller's
/// remaining timeout bounds it too (see `refusal`).
const REFUSED_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// What a resume the backend refused ([`BackendError::StopPending`]) comes to; nothing
/// ran. See `refusal`.
pub(super) enum Refusal
{
    /// The stop the resume was refused for, to report.
    Stop(StopEvent),
    /// It was one `pending_event` drops: drain the rest and resume again.
    Retry,
    /// Nothing came in time: the target died after the refusal and its terminal event is
    /// still on its way. The session runs on, as if the resume had started, and a later
    /// `wait` takes that event.
    Pending,
}

/// Whether `error` is the backend's refusal to resume because a stop arrived after the
/// session's last drain (see [`BackendError::StopPending`]).
pub(super) fn is_stop_pending(error: &DebuggerError) -> bool
{
    matches!(error, DebuggerError::Backend(BackendError::StopPending))
}

impl Session
{
    fn breakpoint_at_pc(&mut self, thread: ThreadId) -> Result<Option<StepSite>>
    {
        let registers = self.backend.read_registers(thread)?;
        let pc = self.arch()?.pc(&registers);
        Ok(self
            .breakpoints
            .id_for_kind(pc, BreakpointKind::Software)
            .and_then(|id| self.breakpoints.get(id))
            .filter(|entry| entry.info.enabled)
            .cloned()
            .map(|entry| StepSite { thread, entry }))
    }

    /// A successful resume consumes held signals for the threads it starts.
    fn resume_backend(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        self.backend.resume(request)?;
        self.carried_signals
            .retain(|(thread, _)| request.action_for(*thread) == ThreadAction::Suspend);
        Ok(())
    }

    pub(super) fn start_continue(&mut self, request: ResumeRequest) -> Result<()>
    {
        if !self
            .breakpoints
            .list()
            .iter()
            .any(|info| info.enabled && info.kind == BreakpointKind::Software)
        {
            return Ok(self.resume_backend(&request)?);
        }
        let mut sites = VecDeque::new();
        for thread in self.threads.clone() {
            if request.action_for(thread) == ThreadAction::Suspend {
                continue;
            }
            if let Some(site) = self.breakpoint_at_pc(thread)? {
                sites.push_back(site);
            }
        }
        if let Some(current) = sites.pop_front() {
            self.start_plan(StepPlan {
                current,
                remaining: sites,
                final_request: Some(request),
                signal_return_sp: None,
            })
        } else {
            Ok(self.resume_backend(&request)?)
        }
    }

    fn start_plan(&mut self, mut plan: StepPlan) -> Result<()>
    {
        let site = &plan.current;
        let mut request = ResumeRequest::step(site.thread);
        let mut rendezvous = None;
        if let Some(final_request) = &mut plan.final_request
            && let Some(signal) = final_request.signal_for(site.thread)
        {
            // Deliver the signal with the original trap still armed. A Step with a
            // signal executes the handler's first instruction, not the instruction
            // under this trap. Wait for the handler to return to this exact frame
            // before removing the trap and executing the original instruction.
            plan.signal_return_sp = Some(self.backend.read_registers(site.thread)?.sp);
            request = ResumeRequest::all(ThreadAction::Suspend)
                .with_thread(site.thread, ThreadAction::Continue)
                .with_signal(site.thread, signal);
            final_request.signals.retain(|(thread, _)| *thread != site.thread);
        } else {
            plan.signal_return_sp = None;
            // Removing the trap gives up a stopped handler's rendezvous there (see
            // `restore_original`), but only once the step really runs.
            rendezvous = self
                .handler_return
                .take_if(|pending| pending.address == site.entry.info.address);
            if let Err(error) = self.restore_original(&site.entry) {
                self.put_back_rendezvous(rendezvous);
                return Err(error);
            }
        }
        if let Err(error) = self.resume_backend(&request) {
            // The backend guarantees that a failed resume ran nothing: with the trap back,
            // the handler can still return to it.
            let rearmed = self.rearm(&site.entry);
            if rearmed.is_ok() {
                self.put_back_rendezvous(rendezvous);
            }
            // A refusal is still a refusal when the trap cannot be put back: the target
            // may have died after it, and the stop it was refused for (its terminal event)
            // is the caller's to report (#155; see `refusal`).
            if error == BackendError::StopPending {
                self.note_refusal_cleanup(rearmed);
            } else {
                rearmed?;
            }
            return Err(error.into());
        }
        self.step_plan = Some(plan);
        Ok(())
    }

    /// Put back the rendezvous `start_plan` took for a step that did not run. `None` means
    /// it took none, and a pending handler return at another address stays as it is.
    fn put_back_rendezvous(&mut self, rendezvous: Option<HandlerReturn>)
    {
        if rendezvous.is_some() {
            self.handler_return = rendezvous;
        }
    }

    /// Start a pending handler return's continue `request` (see `HandlerReturn`) in place
    /// of the user's resume it completes: the session consumed the handler's stop instead
    /// of reporting it. Apply the user's signal policy to the signals still held, including
    /// when the original continue was deferred or a step-over was cut short.
    fn continue_handler(&mut self, request: &ResumeRequest, signal_policy: SignalPolicy) -> Result<Option<StopEvent>>
    {
        self.retry_refused(|session| session.start_continue(session.prepare_signals(request.clone(), signal_policy)))
    }

    /// Drain the events still queued from the last stop, then start the continue of
    /// `request` (see `start_continue`); `Some(event)` instead when one is pending, for
    /// the caller to defer. A stop that arrives after the drain makes the backend refuse
    /// the resume ([`BackendError::StopPending`]); nothing ran, so the pending handler
    /// return is put back as before and that stop is taken (see `refusal`), or the resume
    /// tried again if it was one `pending_event` drops. When that stop does not come in
    /// time (the target died), `None` as for a started continue: a later `wait` takes
    /// the terminal event.
    pub(super) fn continue_after_drain(
        &mut self,
        request: &ResumeRequest,
        signal_policy: SignalPolicy,
    ) -> Result<Option<StopEvent>>
    {
        let deferred = loop {
            if let Some(event) = self.pending_event()? {
                break event;
            }
            self.retry_line_cleanup()?;
            let noted = self.handler_return.as_ref().map(|pending| pending.request.clone());
            let armed = self.handler_exits.is_none();
            self.note_handler_continue(request, signal_policy)?;
            match self.start_continue(request.clone()) {
                Err(error) if is_stop_pending(&error) => {
                    if let (Some(pending), Some(noted)) = (&mut self.handler_return, noted) {
                        pending.request = noted;
                    }
                    if armed {
                        let released = self.release_handler_exits();
                        self.note_refusal_cleanup(released);
                    }
                    match self.refusal()? {
                        Refusal::Stop(event) => break event,
                        Refusal::Retry => {}
                        Refusal::Pending => return Ok(None),
                    }
                }
                started => {
                    self.stale_step = None;
                    if started.is_err() {
                        self.release_handler_exits_after_error();
                    }
                    return started.map(|()| None);
                }
            }
        };
        // If wait consumes this stop as a handler rendezvous, it completes the
        // deferred resume with the user's latest signal policy.
        if let Some(pending) = &mut self.handler_return
            && let Some((_, policy)) = &mut pending.request
        {
            *policy = signal_policy;
        }
        Ok(Some(deferred))
    }

    /// How long a refused resume waits for its stop: `REFUSED_STOP_TIMEOUT`, and inside
    /// [`Session::wait`] no longer than the caller's remaining timeout.
    fn refusal_limit(&self) -> std::time::Duration
    {
        self.wait_deadline.map_or(REFUSED_STOP_TIMEOUT, |deadline| {
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(REFUSED_STOP_TIMEOUT)
        })
    }

    /// Combine a refused resume's outcome with the failure, if any, to put its traps
    /// back: the failure is returned when the resume would be tried again (or no outcome
    /// could be read), and only logged when a stop is reported or the target died.
    fn settle_refusal_cleanup(cleanup: Option<DebuggerError>, refusal: Result<Refusal>) -> Result<Refusal>
    {
        let Some(error) = cleanup else {
            return refusal;
        };
        match refusal {
            Ok(Refusal::Retry) => Err(error),
            Ok(refusal) => {
                tracing::warn!("could not put back the traps of a refused resume: {error}");
                Ok(refusal)
            }
            Err(other) => {
                tracing::warn!("could not put back the traps of a refused resume: {error}");
                Err(other)
            }
        }
    }

    /// Take the stop a resume was refused for. The backend has it queued, so a
    /// zero-timeout wait returns it, except in two cases. It may be an abandoned step's
    /// late completion, which `backend_wait` drops: `Retry`. Or the target died after the
    /// refusal: the macOS backend then drops the dead task's nonterminal events and its
    /// terminal event may still be on its way, so the wait goes on for at most `limit`,
    /// and `Pending` when nothing came.
    fn refused_wait(&mut self, limit: std::time::Duration) -> Result<Refusal>
    {
        let stale = self.stale_step;
        if let Some(event) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            return Ok(Refusal::Stop(event));
        }
        if stale.is_some() && self.stale_step.is_none() {
            return Ok(Refusal::Retry);
        }
        if limit.is_zero() {
            return Ok(Refusal::Pending);
        }
        Ok(self.backend_wait(Some(limit))?.map_or(Refusal::Pending, Refusal::Stop))
    }

    /// The stop the backend refused a resume for ([`BackendError::StopPending`]): it
    /// arrived after the last drain, and the next `wait` returns it. For `resume`,
    /// `step_instruction`, source steps, the next step and final continue of a step-over,
    /// and the internal resumes of a stopped handler's return, which retry the resume
    /// instead of failing: `Retry` when the stop is one `pending_event` drops, the rest of
    /// the queue being drained before the retry, and `Pending` when none came in time
    /// (#155).
    ///
    /// The wait is bounded by `REFUSED_STOP_TIMEOUT` and, inside [`Session::wait`], by the
    /// caller's remaining timeout, so a refused internal resume never blocks `wait` past
    /// it: a dead target's terminal event that is still on its way is left to the next
    /// `wait`.
    ///
    /// A failure to put the resume's traps back after the refusal (see
    /// `note_refusal_cleanup`) does not hide the stop: on a target that died after the
    /// refusal the writes fail, and its terminal event is what the caller must see. The
    /// failure is logged, and returned only when the resume would be tried again.
    pub(super) fn refusal(&mut self) -> Result<Refusal>
    {
        let cleanup = self.refusal_cleanup.take();
        let refusal = self.refused_wait(self.refusal_limit()).and_then(|refusal| match refusal {
            Refusal::Stop(event) if self.stale_trap(None, &event)? => Ok(Refusal::Retry),
            refusal => Ok(refusal),
        });
        Self::settle_refusal_cleanup(cleanup, refusal)
    }

    /// Keep a failure to put back what a resume the backend refused had changed (a trap
    /// removed for a step-over, a temporary trap armed for it) for `refusal`, which takes
    /// the refused stop next. The first failure is kept.
    pub(super) fn note_refusal_cleanup(&mut self, cleanup: Result<()>)
    {
        if let Err(error) = cleanup {
            if self.refusal_cleanup.is_none() {
                self.refusal_cleanup = Some(error);
            } else {
                tracing::warn!("could not put back the traps of a refused resume: {error}");
            }
        }
    }

    /// Start an internal resume with `start`: `None` when it
    /// started, or was refused and no stop came in time (the target died: a later `wait`
    /// takes its terminal event, #155). Otherwise the stop it was refused for, to report
    /// instead, unless it is one `pending_event` drops (see `refusal`): the rest of the
    /// queue is drained and the resume started again (#165). A refused start ran nothing
    /// and left the rendezvous, carried signals and exit traps as they were.
    ///
    /// A started resume clears `stale_step`, as in `continue_after_drain`: the backend
    /// starts a resume only with no stop queued, and it clears every thread's step bit
    /// first, so an abandoned step can no longer complete. The next `SingleStep` then
    /// completes the new step, including a step over a breakpoint on the same thread
    /// (#167).
    fn retry_refused(&mut self, mut start: impl FnMut(&mut Self) -> Result<()>) -> Result<Option<StopEvent>>
    {
        loop {
            match start(self) {
                Ok(()) => {
                    self.stale_step = None;
                    return Ok(None);
                }
                Err(error) if is_stop_pending(&error) => match self.refusal()? {
                    Refusal::Stop(event) => return Ok(Some(event)),
                    Refusal::Retry => {
                        if let Some(event) = self.pending_event()? {
                            return Ok(Some(event));
                        }
                    }
                    Refusal::Pending => return Ok(None),
                },
                Err(error) => return Err(error),
            }
        }
    }

    /// Start the next step of an internal step-over `plan`. If a stop arrived first, the
    /// backend runs nothing. A reportable stop ends the plan and is returned. When
    /// `pending_event` drops the stop (see `refusal`), retry after draining the queue
    /// (#158). If the target died and no stop comes in time, drop the plan and return
    /// `None`. A later `wait` takes the terminal event (#155).
    fn continue_plan(&mut self, plan: StepPlan) -> Result<Option<StopEvent>>
    {
        // A refused start discards its copy of the plan. A retry still needs that
        // thread's signal in the final request.
        self.retry_refused(|session| session.start_plan(plan.clone()))
    }

    fn rearm(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        let trap = self.arch()?.trap_instruction();
        if let Err(error) = self.backend.write_memory(entry.info.address, trap) {
            // Memory still contains originals. Disable the entry and all logical owners.
            self.mark_disabled(entry.info.id);
            return Err(error.into());
        }
        Ok(())
    }

    /// Execute one instruction of `thread`, leaving every other thread suspended.
    ///
    /// Requires a stopped process. A pending event defers the step exactly as for
    /// [`resume`](Self::resume): the session becomes running, and the next
    /// [`wait`](Self::wait) returns that event without executing an instruction. That
    /// includes a stop the backend refuses the step for because it arrived after the
    /// drain. A pending breakpoint stop at an address that no longer holds a trap is
    /// dropped as for `resume`.
    /// At an enabled software breakpoint its original instruction is executed and the
    /// trap is restored before the stop is returned. Completion is reported as
    /// [`StopEvent::SingleStep`]. Other events are reported normally. The stepping
    /// thread's held signal is discarded when the step starts it; a deferred or failed
    /// start keeps it held. Peers keep their held signals.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::ThreadNotFound`] for an unknown thread, or a
    /// backend error. Failed starts follow the trap-restoration rules of
    /// [`resume`](Self::resume) and stay stopped. A failed re-arm disables the address
    /// entry and every logical owner; their other sites may remain armed.
    pub fn step_instruction(&mut self, thread: ThreadId) -> Result<()>
    {
        self.require_stopped()?;
        if !self.threads.contains(&thread) {
            return Err(DebuggerError::ThreadNotFound(thread.0));
        }
        self.carry_last_signal();
        self.interrupt_requested = false;
        // An instruction step reports a handler's return like any other stop.
        self.handler_return = None;
        loop {
            if let Some(event) = self.pending_event()? {
                self.deferred = Some(event);
                return self.enter_running();
            }
            self.retry_line_cleanup()?;
            match self.step_thread(thread) {
                // A stop arrived after the drain and nothing ran: report it, or step again
                // if it was one `pending_event` drops. If none comes in time the target
                // died: the session runs, and a later `wait` takes its terminal event.
                Err(error) if is_stop_pending(&error) => match self.refusal()? {
                    Refusal::Stop(event) => {
                        self.deferred = Some(event);
                        return self.enter_running();
                    }
                    Refusal::Retry => {}
                    Refusal::Pending => return self.enter_running(),
                },
                started => {
                    self.stale_step = None;
                    started?;
                    return self.enter_running();
                }
            }
        }
    }

    /// Start one instruction step of `thread` with the others suspended, stepping over an
    /// enabled software breakpoint at its pc; its completion is a `SingleStep` of `thread`.
    fn step_thread(&mut self, thread: ThreadId) -> Result<()>
    {
        if let Some(current) = self.breakpoint_at_pc(thread)? {
            self.start_plan(StepPlan {
                current,
                remaining: VecDeque::new(),
                final_request: None,
                signal_return_sp: None,
            })
        } else {
            Ok(self.resume_backend(&ResumeRequest::step(thread))?)
        }
    }

    fn enter_running(&mut self) -> Result<()>
    {
        self.state = SessionState::Running;
        self.last_stop = None;
        Ok(())
    }

    /// An event still queued from the last stop, taken without blocking, for `resume`
    /// and `step_instruction`. A breakpoint stop of a private trap that is gone (see
    /// `stale_trap`) belongs to no request of the caller and is dropped; the thread has
    /// not executed the instruction there yet, so the resume runs it. Draining goes on
    /// until a reportable event or nothing is left.
    pub(super) fn pending_event(&mut self) -> Result<Option<StopEvent>>
    {
        while let Some(event) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            if !self.stale_trap(None, &event)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// `Backend::wait`, minus the `SingleStep` of an abandoned internal step (see
    /// `stale_step`), which belongs to no request of the caller.
    pub(super) fn backend_wait(&mut self, timeout: Option<std::time::Duration>) -> Result<Option<StopEvent>>
    {
        loop {
            let event = self.backend.wait(timeout)?;
            if event.is_some() {
                self.stop_generation = self.stop_generation.wrapping_add(1);
            }
            match (&event, self.stale_step) {
                (Some(StopEvent::SingleStep { thread }), Some(stale)) if *thread == stale => {
                    self.stale_step = None;
                }
                _ => return Ok(event),
            }
        }
    }

    /// Drive the step-overs started by `resume` until the final continue is issued, so the
    /// program runs without the caller calling `wait`.
    ///
    /// Each step is awaited for at most [`STEP_OVER_TIMEOUT`](super::STEP_OVER_TIMEOUT);
    /// on a timeout the sequence is left to [`wait`](Self::wait). Another event (the step
    /// did not complete) or a backend `wait` error is kept for the next `wait`. A failure
    /// to start the next step or the final continue stops the session, as in `wait`.
    pub(super) fn drive_step_overs(&mut self) -> Result<()>
    {
        while self.step_plan.as_ref().is_some_and(|plan| plan.final_request.is_some()) {
            let event = match self.backend_wait(Some(super::STEP_OVER_TIMEOUT)) {
                Ok(Some(event)) => event,
                Ok(None) => return Ok(()),
                Err(error) => {
                    self.deferred_error = Some(error);
                    return Ok(());
                }
            };
            match self.finish_step(event) {
                Ok(None) => {}
                Ok(Some(event)) => {
                    self.deferred = Some(event);
                    return Ok(());
                }
                Err(error) => {
                    self.state = SessionState::Stopped;
                    self.sync_threads(None);
                    self.last_stop = None;
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// None means an internal completion was consumed and execution resumed.
    pub(super) fn finish_step(&mut self, event: StopEvent) -> Result<Option<StopEvent>>
    {
        let event = match self.finish_breakpoint_step(event) {
            Ok(event) => event,
            Err(error) => {
                if let Err(cleanup) = self.cancel_line_step() {
                    tracing::warn!("could not cancel a source step: {cleanup}");
                }
                self.release_handler_exits_after_error();
                return Err(error);
            }
        };
        match event {
            Some(event) if self.line_plan.is_none() => self.finish_handler_return(event),
            Some(event) => self.finish_line_step(event),
            None => Ok(None),
        }
    }

    /// Note the continue `request` for a pending handler return (see `HandlerReturn`), and
    /// trap the non-local exits of its thread for the continue, so a handler that leaves
    /// by `siglongjmp` gives the rendezvous up. Requires a stopped session.
    fn note_handler_continue(&mut self, request: &ResumeRequest, signal_policy: SignalPolicy) -> Result<()>
    {
        let Some(pending) = &mut self.handler_return else {
            return Ok(());
        };
        let mut request = request.clone();
        request.signals.clear();
        pending.request = Some((request, signal_policy));
        let thread = pending.thread;
        if self.handler_exits.is_none() {
            let mut exits = None;
            let armed = self.arm_exit_hooks(&mut exits, thread);
            if let Err(error) = armed {
                if let Err(cleanup) = self.release_exit_hooks(exits) {
                    tracing::warn!("could not remove a temporary exit breakpoint: {cleanup}");
                }
                return Err(error);
            }
            self.handler_exits = exits;
        }
        Ok(())
    }

    /// Remove the exit traps of a pending handler return. They exist only while a continue
    /// runs; the backend is stopped whenever this is called.
    fn release_handler_exits(&mut self) -> Result<()>
    {
        let Some(exits) = self.handler_exits.take() else {
            return Ok(());
        };
        let state = std::mem::replace(&mut self.state, SessionState::Stopped);
        let result = self.release_exit_hooks(Some(exits));
        self.state = state;
        result
    }

    /// Remove the exit traps of a pending handler return, logging a failure.
    pub(super) fn release_handler_exits_after_error(&mut self)
    {
        if let Err(cleanup) = self.release_handler_exits() {
            tracing::warn!("could not remove a temporary exit breakpoint: {cleanup}");
        }
    }

    /// A breakpoint stop that is a stopped handler's return to its step-resume trap, in the
    /// frame it interrupted, is consumed: the original instruction is stepped over and the
    /// user's continue reissued. So are the stops of the exit traps armed for the continue.
    /// Any other event is returned, and the exit traps are removed with it.
    fn finish_handler_return(&mut self, event: StopEvent) -> Result<Option<StopEvent>>
    {
        if event.is_terminal() {
            self.handler_return = None;
            self.handler_exits = None;
            return Ok(Some(event));
        }
        let result = self.handler_return_stop(event);
        if !matches!(result, Ok(None))
            && let Err(cleanup) = self.release_handler_exits()
        {
            if result.is_ok() {
                return Err(cleanup);
            }
            tracing::warn!("could not remove a temporary exit breakpoint: {cleanup}");
        }
        result
    }

    fn handler_return_stop(&mut self, event: StopEvent) -> Result<Option<StopEvent>>
    {
        if let Some(consumed) = self.handler_exit_stop(&event)? {
            return Ok(consumed);
        }
        let Some(pending) = &self.handler_return else {
            return Ok(Some(event));
        };
        let StopEvent::Breakpoint { thread, pc } = event else {
            return Ok(Some(event));
        };
        if thread != pending.thread || self.arch()?.pc_after_trap(pc) != pending.address {
            return Ok(Some(event));
        }
        let Some((request, signal_policy)) = pending.request.clone() else {
            return Ok(Some(event));
        };
        if self.backend.read_registers(thread)?.sp != pending.sp {
            return Ok(Some(event));
        }
        self.handler_return = None;
        if self.interrupt_requested {
            return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
        }
        // Other threads' events reported with this stop are the caller's, except a
        // breakpoint stop of a private trap that is gone (see `pending_event`), which
        // belongs to no request and is dropped (#165).
        if let Some(other) = self.pending_event()? {
            return Ok(Some(other));
        }
        // The rendezvous is over, and its exit traps with it.
        self.release_handler_exits()?;
        self.continue_handler(&request, signal_policy)
    }

    /// Consume `event` if it belongs to the exit traps of a pending handler return:
    /// `Some(None)` when it was consumed and execution goes on, `Some(Some(event))` for an
    /// event to report instead, `None` when it is not theirs.
    ///
    /// The handler's thread at an exit entry is single-stepped alone until it lands. A
    /// landing in the frame the handler interrupted or an outer one means the handler is
    /// gone without returning (`siglongjmp`), so the rendezvous is given up. Another thread
    /// at an exit trap steps over it.
    fn handler_exit_stop(&mut self, event: &StopEvent) -> Result<Option<Option<StopEvent>>>
    {
        let (Some(exits), Some(pending)) = (&self.handler_exits, &self.handler_return) else {
            return Ok(None);
        };
        let Some((request, signal_policy)) = pending.request.clone() else {
            return Ok(None);
        };
        let (thread, interrupted_sp) = (exits.thread(), pending.sp);
        let hit = self.exit_hook_hit(exits, event)?;
        let stepped = exits.leaving() && *event == (StopEvent::SingleStep { thread });
        if hit.is_none() && !stepped {
            return Ok(None);
        }
        if self.interrupt_requested {
            return Ok(Some(Some(StopEvent::Interrupted {
                thread: event.thread().or(Some(thread)),
            })));
        }
        // Other threads' events reported with this stop are the caller's, except a
        // breakpoint stop of a private trap that is gone (see `pending_event`), which is
        // dropped (#165).
        if let Some(other) = self.pending_event()? {
            return Ok(Some(Some(other)));
        }
        let Some(mut exits) = self.handler_exits.take() else {
            return Ok(None);
        };
        // Adding and removing traps needs a stopped session; the backend is stopped.
        let state = std::mem::replace(&mut self.state, SessionState::Stopped);
        let result = self.follow_handler_exit(&mut exits, hit, interrupted_sp).and_then(|follow| {
            if self.handler_return.is_none() {
                // A given-up rendezvous takes its exit traps with it, before the process
                // runs again.
                self.release_exit_hooks(Some(std::mem::replace(&mut exits, ExitHooks::empty(thread))))?;
            }
            match follow {
                Follow::Step => self.retry_refused(|session| session.step_thread(thread)),
                Follow::Continue => self.continue_handler(&request, signal_policy),
                Follow::Report(event) => Ok(Some(event)),
            }
        });
        self.state = state;
        self.handler_exits = self.handler_return.is_some().then_some(exits);
        result.map(Some)
    }

    /// What to do after a stop of a pending handler return's exit traps.
    fn follow_handler_exit(
        &mut self,
        exits: &mut ExitHooks,
        hit: Option<(ThreadId, usize)>,
        interrupted_sp: Address,
    ) -> Result<Follow>
    {
        let thread = exits.thread();
        match hit {
            Some((hit_thread, image)) if hit_thread == thread => {
                self.begin_exit(exits, image)?;
                return Ok(Follow::Step);
            }
            // Another thread at an exit trap: the continue steps it over the trap.
            Some(_) => return Ok(Follow::Continue),
            None => {}
        }
        match self.exit_stepped(exits)? {
            ExitStep::Stepping => Ok(Follow::Step),
            ExitStep::Landed { pc, sp } => {
                if sp >= interrupted_sp {
                    // The handler left the frame it interrupted without returning.
                    self.handler_return = None;
                }
                // A user breakpoint at the landing is that breakpoint's hit; the continue
                // must not step over it.
                if let Some(id) = self.breakpoints.id_for_kind(pc, BreakpointKind::Software)
                    && self.breakpoints.get(id).is_some_and(|entry| entry.info.enabled)
                {
                    return Ok(Follow::Report(StopEvent::Breakpoint { thread, pc }));
                }
                Ok(Follow::Continue)
            }
            ExitStep::Lost => Ok(Follow::Continue),
        }
    }

    fn finish_breakpoint_step(&mut self, event: StopEvent) -> Result<Option<StopEvent>>
    {
        let Some(mut plan) = self.step_plan.take() else {
            return Ok(Some(event));
        };
        if event.is_terminal() {
            return Ok(Some(event));
        }
        if let Some(sp) = plan.signal_return_sp {
            if let StopEvent::Breakpoint { thread, pc } = event
                && thread == plan.current.thread
                && self.arch()?.pc_after_trap(pc) == plan.current.entry.info.address
                && self.backend.read_registers(thread)?.sp == sp
            {
                // This trap is the step-resume rendezvous, not another user hit.
                plan.signal_return_sp = None;
                return self.continue_plan(plan);
            }
            // The handler stopped before returning. Remember the rendezvous so its later
            // return to this trap, in this frame, is not reported as a second hit.
            self.handler_return = Some(HandlerReturn {
                thread: plan.current.thread,
                address: plan.current.entry.info.address,
                sp,
                request: None,
            });
            return Ok(Some(event));
        }
        self.rearm(&plan.current.entry)?;
        if event
            != (StopEvent::SingleStep {
                thread: plan.current.thread,
            })
        {
            self.stale_step = Some(plan.current.thread);
            return Ok(Some(event));
        }
        // A step the caller asked for is reported as such, interrupt or not.
        if plan.final_request.is_none() {
            return Ok(Some(event));
        }
        if self.interrupt_requested {
            let thread = plan.current.thread;
            return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
        }
        // The stop that completed the step may have reported other threads' events too;
        // the backend refuses to resume until they are drained, and they are the caller's,
        // except a breakpoint stop of a private trap that is gone (see `pending_event`),
        // which belongs to no request and is dropped (#158).
        if let Some(pending) = self.pending_event()? {
            return Ok(Some(pending));
        }
        if let Some(next) = plan.remaining.pop_front() {
            plan.current = next;
            return self.continue_plan(plan);
        }
        let Some(request) = plan.final_request else {
            return Ok(None);
        };
        self.retry_refused(|session| Ok(session.resume_backend(&request)?))
    }
}
