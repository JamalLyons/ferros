//! Resumable, serial software-breakpoint step-overs.

use std::collections::VecDeque;

use super::{Session, SessionState};
use crate::backend::{ResumeRequest, StopEvent};
use crate::breakpoints::{BreakpointEntry, BreakpointKind};
use crate::error::{DebuggerError, Result};
use crate::types::ThreadId;

pub(super) struct StepSite
{
    thread: ThreadId,
    entry: BreakpointEntry,
}

pub(super) struct StepPlan
{
    current: StepSite,
    remaining: VecDeque<StepSite>,
    // None means the user requested a single step: report its completion.
    final_request: Option<ResumeRequest>,
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
            if let Some(site) = self.breakpoint_at_pc(thread)? {
                sites.push_back(site);
            }
        }
        if let Some(current) = sites.pop_front() {
            self.start_plan(StepPlan {
                current,
                remaining: sites,
                final_request: Some(request),
            })
        } else {
            Ok(self.backend.resume(&request)?)
        }
    }

    fn start_plan(&mut self, mut plan: StepPlan) -> Result<()>
    {
        let site = &plan.current;
        self.restore_original(&site.entry)?;
        let mut request = ResumeRequest::step(site.thread);
        if let Some(final_request) = &mut plan.final_request
            && let Some(signal) = final_request.signal_for(site.thread)
        {
            request = request.with_signal(site.thread, signal);
            final_request.signals.retain(|(thread, _)| *thread != site.thread);
        }
        if let Err(error) = self.backend.resume(&request) {
            // The backend guarantees that a failed resume ran nothing.
            self.rearm(&site.entry)?;
            return Err(error.into());
        }
        self.step_plan = Some(plan);
        Ok(())
    }

    fn rearm(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        let trap = self.arch()?.trap_instruction();
        if let Err(error) = self.backend.write_memory(entry.info.address, trap) {
            // Memory still contains originals. Keep the table honest if rearming fails.
            self.mark_disabled(entry.info.id);
            return Err(error.into());
        }
        Ok(())
    }

    /// Execute one instruction of `thread`, leaving every other thread suspended.
    ///
    /// Requires a stopped process. A pending event defers the step exactly as for
    /// [`resume`](Self::resume): the session becomes running, and the next
    /// [`wait`](Self::wait) returns that event without executing an instruction.
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
    /// [`resume`](Self::resume) and stay stopped.
    pub fn step_instruction(&mut self, thread: ThreadId) -> Result<()>
    {
        self.require_stopped()?;
        if !self.threads.contains(&thread) {
            return Err(DebuggerError::ThreadNotFound(thread.0));
        }
        self.interrupt_requested = false;
        if let Some(event) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            self.deferred = Some(event);
            return self.enter_running();
        }
        self.stale_step = None;
        // The stepping thread's held signal is discarded by this step; other threads stay
        // suspended, so a signal carried for them is still held.
        self.carried_signals.retain(|(carried, _)| *carried != thread);
        if let Some(current) = self.breakpoint_at_pc(thread)? {
            self.start_plan(StepPlan {
                current,
                remaining: VecDeque::new(),
                final_request: None,
            })?;
        } else {
            self.backend.resume(&ResumeRequest::step(thread))?;
        }
        self.enter_running()
    }

    fn enter_running(&mut self) -> Result<()>
    {
        self.state = SessionState::Running;
        self.last_stop = None;
        Ok(())
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
            self.carried_signals = final_request.signals;
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
        let Some(mut plan) = self.step_plan.take() else {
            return Ok(Some(event));
        };
        if event.is_terminal() {
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
            self.start_plan(plan)?;
        } else if let Some(request) = plan.final_request {
            self.backend.resume(&request)?;
        }
        Ok(None)
    }
}
