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
        if let Some(event) = self.backend.wait(Some(std::time::Duration::ZERO))? {
            self.deferred = Some(event);
        } else if let Some(current) = self.breakpoint_at_pc(thread)? {
            self.start_plan(StepPlan {
                current,
                remaining: VecDeque::new(),
                final_request: None,
            })?;
        } else {
            self.backend.resume(&ResumeRequest::step(thread))?;
        }
        self.state = SessionState::Running;
        self.last_stop = None;
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
            return Ok(Some(event));
        }
        if self.interrupt_requested {
            return Ok(Some(StopEvent::Interrupted {
                thread: Some(plan.current.thread),
            }));
        }
        if plan.final_request.is_none() {
            return Ok(Some(event));
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
