//! Resumable, serial software-breakpoint step-overs.

use std::collections::VecDeque;

use super::exits::{ExitHooks, ExitStep};
use super::{Session, SessionState};
use crate::backend::{BackendError, ResumeRequest, StopEvent, ThreadAction};
use crate::breakpoints::{BreakpointEntry, BreakpointKind};
use crate::error::{DebuggerError, Result};
use crate::types::{Address, ThreadId};

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
    /// The continue the user resumed with since the handler stopped (signals removed: they
    /// were delivered then), reissued after stepping over the original instruction. `None`
    /// until a plain continue runs the handler on.
    request: Option<ResumeRequest>,
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

pub(super) struct StepPlan
{
    current: StepSite,
    remaining: VecDeque<StepSite>,
    // None means the user requested a single step: report its completion.
    final_request: Option<ResumeRequest>,
    signal_return_sp: Option<Address>,
}

/// How long `refused_stop` waits for the stop a resume was refused for. The backend has
/// it queued already, so this bounds only the case where the target died meanwhile and
/// its terminal event is still being collected.
const REFUSED_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

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

    pub(super) fn start_continue(&mut self, request: ResumeRequest) -> Result<()>
    {
        if !self
            .breakpoints
            .list()
            .iter()
            .any(|info| info.enabled && info.kind == BreakpointKind::Software)
        {
            return Ok(self.backend.resume(&request)?);
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
            Ok(self.backend.resume(&request)?)
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
        if let Err(error) = self.backend.resume(&request) {
            // The backend guarantees that a failed resume ran nothing: with the trap back,
            // the handler can still return to it.
            self.rearm(&site.entry)?;
            self.put_back_rendezvous(rendezvous);
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
    /// of reporting it. Signals still carried from that resume (it was deferred for the
    /// stop, or a step-over cut short) are passed with it to the threads it lets run, as
    /// the resume would have; otherwise they are lost when those threads run on, and a
    /// later `resume_with_signal` would pass them to threads that no longer hold them.
    /// Once the continue starts they are no longer carried; a refused or failed start ran
    /// nothing, so they stay carried.
    fn continue_handler(&mut self, mut request: ResumeRequest) -> Result<Option<StopEvent>>
    {
        let mut passed = Vec::new();
        for &(thread, signal) in &self.carried_signals {
            if request.action_for(thread) != ThreadAction::Suspend {
                merge_signal(&mut request.signals, thread, signal);
                passed.push(thread);
            }
        }
        let started = self.start_continue(request);
        if started.is_ok() {
            self.carried_signals.retain(|(thread, _)| !passed.contains(thread));
        }
        self.or_refused_stop(started)
    }

    /// Drain the events still queued from the last stop, then start the continue of
    /// `request` (see `start_continue`); `Some(event)` instead when one is pending, for
    /// the caller to defer. A stop that arrives after the drain makes the backend refuse
    /// the resume ([`BackendError::StopPending`]); nothing ran, so the pending handler
    /// return is put back as before and that stop is taken (see `refused_stop`), or the
    /// resume tried again if it was one `pending_event` drops.
    pub(super) fn continue_after_drain(&mut self, request: &ResumeRequest) -> Result<Option<StopEvent>>
    {
        loop {
            if let Some(event) = self.pending_event()? {
                return Ok(Some(event));
            }
            let noted = self.handler_return.as_ref().map(|pending| pending.request.clone());
            let armed = self.handler_exits.is_none();
            self.note_handler_continue(request)?;
            match self.start_continue(request.clone()) {
                Err(error) if is_stop_pending(&error) => {
                    if let (Some(pending), Some(noted)) = (&mut self.handler_return, noted) {
                        pending.request = noted;
                    }
                    if armed {
                        self.release_handler_exits()?;
                    }
                    if let Some(event) = self.refused_event()? {
                        return Ok(Some(event));
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
        }
    }

    /// Keep the `signals` of a request that was not sent for the next
    /// `resume_with_signal`, merged into those already carried: a thread has at most one
    /// carried signal, the latest.
    pub(super) fn carry_signals(&mut self, signals: Vec<(ThreadId, i32)>)
    {
        for (thread, signal) in signals {
            merge_signal(&mut self.carried_signals, thread, signal);
        }
    }

    /// The stop the backend refused a resume for ([`BackendError::StopPending`]): it
    /// arrived after the last drain, and the next `wait` returns it.
    ///
    /// If nothing comes (see `refused_wait`) the refusal is returned as an error; nothing
    /// ran and the session is unchanged.
    pub(super) fn refused_stop(&mut self) -> Result<StopEvent>
    {
        self.refused_wait()?.ok_or(DebuggerError::Backend(BackendError::StopPending))
    }

    /// Take the stop a resume was refused for. The backend has it queued, so a
    /// zero-timeout wait returns it, except in two cases. It may be an abandoned step's
    /// late completion, which `backend_wait` drops: `None`. Or the target died after the
    /// refusal: the macOS backend then drops the dead task's nonterminal events and its
    /// terminal event may still be on its way, so the wait goes on for at most
    /// `REFUSED_STOP_TIMEOUT`.
    fn refused_wait(&mut self) -> Result<Option<StopEvent>>
    {
        let stale = self.stale_step;
        if let Some(event) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            return Ok(Some(event));
        }
        if stale.is_some() && self.stale_step.is_none() {
            return Ok(None);
        }
        self.backend_wait(Some(REFUSED_STOP_TIMEOUT))
    }

    /// As `refused_stop`, for `resume` and `step_instruction`, which retry the resume
    /// instead of failing: `None` when the stop is one `pending_event` drops (or none
    /// came), the rest of the queue being drained before the retry.
    fn refused_event(&mut self) -> Result<Option<StopEvent>>
    {
        match self.refused_wait()? {
            Some(event) if !self.stale_trap(None, &event)? => Ok(Some(event)),
            _ => Ok(None),
        }
    }

    /// `None` when an internal resume `started`; the stop it was refused for (see
    /// `refused_stop`), to report instead, or its error otherwise.
    pub(super) fn or_refused_stop(&mut self, started: Result<()>) -> Result<Option<StopEvent>>
    {
        match started {
            Ok(()) => Ok(None),
            Err(error) if is_stop_pending(&error) => self.refused_stop().map(Some),
            Err(error) => Err(error),
        }
    }

    /// Start the next step of an internal step-over `plan`. If a stop arrived first, the
    /// backend runs nothing: the plan is given up as in `abandon` (no step was started, so
    /// no completion can follow) and that stop is returned.
    fn continue_plan(&mut self, plan: StepPlan) -> Result<Option<StopEvent>>
    {
        // Cloned before `start_plan` moves the current thread's signal out of the request.
        let carried = plan.final_request.as_ref().map(|request| request.signals.clone());
        let started = self.start_plan(plan);
        if let (Err(error), Some(signals)) = (&started, carried)
            && is_stop_pending(error)
        {
            self.carry_signals(signals);
        }
        self.or_refused_stop(started)
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
    /// [`StopEvent::SingleStep`]; other events are reported normally. A held signal
    /// of this thread is discarded; signals of suspended threads remain held.
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
        self.interrupt_requested = false;
        // An instruction step reports a handler's return like any other stop.
        self.handler_return = None;
        loop {
            if let Some(event) = self.pending_event()? {
                self.deferred = Some(event);
                return self.enter_running();
            }
            match self.step_thread(thread) {
                // A stop arrived after the drain and nothing ran: report it, or step again
                // if it was one `pending_event` drops.
                Err(error) if is_stop_pending(&error) => {
                    if let Some(event) = self.refused_event()? {
                        self.deferred = Some(event);
                        return self.enter_running();
                    }
                }
                started => {
                    self.stale_step = None;
                    // A failed start ran nothing, so the thread's held signal is still held.
                    started?;
                    // The stepping thread's held signal is discarded by this step; other
                    // threads stay suspended, so a signal carried for them is still held.
                    self.carried_signals.retain(|(carried, _)| *carried != thread);
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
            Ok(self.backend.resume(&ResumeRequest::step(thread))?)
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
            match (&event, self.stale_step) {
                (Some(StopEvent::SingleStep { thread }), Some(stale)) if *thread == stale => {
                    self.stale_step = None;
                }
                _ => return Ok(event),
            }
        }
    }

    /// Give up the rest of `plan` because a stop other than its step completion is
    /// reported: a late `SingleStep` of the step is dropped, and signals meant for the
    /// final continue are carried to the next `resume_with_signal`.
    fn abandon(&mut self, plan: StepPlan, step_completed: bool)
    {
        if !step_completed {
            self.stale_step = Some(plan.current.thread);
        }
        if let Some(final_request) = plan.final_request {
            self.carry_signals(final_request.signals);
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
    pub(super) fn note_handler_continue(&mut self, request: &ResumeRequest) -> Result<()>
    {
        let Some(pending) = &mut self.handler_return else {
            return Ok(());
        };
        let mut request = request.clone();
        request.signals.clear();
        pending.request = Some(request);
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
        let Some(request) = pending.request.clone() else {
            return Ok(Some(event));
        };
        if self.backend.read_registers(thread)?.sp != pending.sp {
            return Ok(Some(event));
        }
        self.handler_return = None;
        if self.interrupt_requested {
            return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
        }
        // Other threads' events reported with this stop are the caller's.
        if let Some(other) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            return Ok(Some(other));
        }
        // The rendezvous is over, and its exit traps with it.
        self.release_handler_exits()?;
        self.continue_handler(request)
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
        let Some(request) = pending.request.clone() else {
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
        // Other threads' events reported with this stop are the caller's.
        if let Some(other) = self.backend_wait(Some(std::time::Duration::ZERO))? {
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
                Follow::Step => {
                    let started = self.step_thread(thread);
                    self.or_refused_stop(started)
                }
                Follow::Continue => self.continue_handler(request),
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
            // No instruction step was started, so there can be no stale completion.
            self.abandon(plan, true);
            return Ok(Some(event));
        }
        self.rearm(&plan.current.entry)?;
        if event
            != (StopEvent::SingleStep {
                thread: plan.current.thread,
            })
        {
            self.abandon(plan, false);
            return Ok(Some(event));
        }
        // A step the caller asked for is reported as such, interrupt or not.
        if plan.final_request.is_none() {
            return Ok(Some(event));
        }
        if self.interrupt_requested {
            let thread = plan.current.thread;
            self.abandon(plan, true);
            return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
        }
        // The stop that completed the step may have reported other threads' events too;
        // the backend refuses to resume until they are drained, and they are the caller's.
        if let Some(pending) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            self.abandon(plan, true);
            return Ok(Some(pending));
        }
        if let Some(next) = plan.remaining.pop_front() {
            plan.current = next;
            return self.continue_plan(plan);
        }
        if let Some(request) = plan.final_request
            && let Err(error) = self.backend.resume(&request)
        {
            if error != BackendError::StopPending {
                return Err(error.into());
            }
            // A stop arrived after the drain; the final continue's signals stay held.
            self.carry_signals(request.signals);
            return self.refused_stop().map(Some);
        }
        Ok(None)
    }
}

/// Set `thread`'s entry in `signals` to `signal`, replacing an earlier one.
pub(super) fn merge_signal(signals: &mut Vec<(ThreadId, i32)>, thread: ThreadId, signal: i32)
{
    signals.retain(|(carried, _)| *carried != thread);
    signals.push((thread, signal));
}
