//! Source-line plans layered over instruction steps and private return breakpoints.

use std::ops::Range;

use super::exits::{ExitHooks, ExitStep, SiteOwner};
use super::{Session, SessionState, SourcePosition};
use crate::backend::{ResumeRequest, StopEvent, ThreadAction};
use crate::breakpoints::{BreakpointId, BreakpointKind};
use crate::error::{DebuggerError, Result};
use crate::types::{Address, ThreadId};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode
{
    Over,
    Into,
    Out,
}

struct RunTo
{
    address: Address,
    sp: Address,
    id: BreakpointId,
    owner: SiteOwner,
}

pub(super) struct LinePlan
{
    thread: ThreadId,
    mode: Mode,
    position: SourcePosition,
    ranges: Vec<Range<Address>>,
    call: Option<(Address, Address)>,
    prologue_end: Option<Address>,
    run_to: Option<RunTo>,
    /// Traps on non-local exits, armed while a call runs to its return.
    exits: Option<ExitHooks>,
}

impl Session
{
    /// Advance the selected thread to the next source line, stepping over calls.
    ///
    /// Other threads remain suspended for the entire operation, including while a
    /// temporary return breakpoint runs the selected thread freely. Calls that wait
    /// for another thread can therefore block; use [`interrupt`](Self::interrupt).
    /// The session becomes running; [`wait`](Self::wait) reports `SingleStep` at the
    /// new line, a user breakpoint, another intervening stop, or process exit.
    /// Without line information at the starting pc this executes one instruction.
    ///
    /// Known limits:
    /// - A call is recognised by `BL`/`BLR`; pointer-authenticated calls (`BLRAA` and
    ///   relatives, arm64e only) are stepped into one instruction at a time.
    /// - Only `is_stmt` line-table rows start a line: the statement being stepped runs
    ///   on through rows without it that follow it. Leaving that statement's code for
    ///   another line still stops wherever it lands, statement row or not.
    /// - The return trap is matched by stack pointer. A callee that leaves its caller
    ///   without returning, by unwinding through the system unwinder (a panic caught by an
    ///   outer `catch_unwind`) or by `longjmp`/`_longjmp`/`siglongjmp`, is caught where
    ///   that exit lands: a landing in the stepping frame or an outer one ends the step
    ///   there (reported as `SingleStep`, possibly at code without a source line).
    /// - Inlined callees are stepped over, never entered.
    ///
    /// # Errors
    ///
    /// As for [`step_instruction`](Self::step_instruction), plus errors reading debug
    /// information or installing a temporary breakpoint. Requires a selected thread.
    pub fn step_over(&mut self) -> Result<()>
    {
        self.start_line_step(Mode::Over)
    }

    /// Advance the selected thread into a call that has source information.
    ///
    /// Stops after the callee's prologue, found from the function whose code holds the
    /// callee's first instruction (so trait methods and closures are entered too). Calls
    /// without line information are run to their return address. Other threads stay suspended, with the same blocking,
    /// stop reporting and no-line fallback policy as [`step_over`](Self::step_over).
    ///
    /// # Errors
    ///
    /// As for [`step_over`](Self::step_over).
    pub fn step_into(&mut self) -> Result<()>
    {
        self.start_line_step(Mode::Into)
    }

    /// Run the selected thread to its caller, using the caller's SP to distinguish
    /// recursive invocations of the same return address. Inside an inlined frame this
    /// returns to the physical caller, past the inlined function's own caller.
    ///
    /// Other threads stay suspended. [`wait`](Self::wait) reports `SingleStep` at
    /// the return pc, a user breakpoint, another intervening stop, or process exit.
    ///
    /// # Errors
    ///
    /// As for [`step_over`](Self::step_over), plus [`DebuggerError::InvalidArgument`]
    /// when unwinding cannot recover a caller.
    pub fn step_out(&mut self) -> Result<()>
    {
        self.start_line_step(Mode::Out)
    }

    fn start_line_step(&mut self, mode: Mode) -> Result<()>
    {
        self.require_stopped()?;
        let thread = self.selected_or_error()?;
        self.interrupt_requested = false;
        if let Some(event) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            // The session is stopped, so a queued single-step completion of this thread
            // (an interrupted step's late completion) or a trap from a return breakpoint
            // that is already gone belongs to no request; drop it and start the step.
            // Anything else is reported first.
            let stale_step = event == (StopEvent::SingleStep { thread });
            if !stale_step && !self.stale_trap(Some(thread), &event)? {
                self.deferred = Some(event);
                self.state = SessionState::Running;
                self.last_stop = None;
                return Ok(());
            }
        }
        self.stale_step = None;
        // A source step reports a stopped handler's return like any other stop.
        self.handler_return = None;
        self.carried_signals.retain(|(carried, _)| *carried != thread);
        let registers = self.backend.read_registers(thread)?;
        let position = self.position_at(registers.pc, None);
        let ranges = self.line_ranges(registers.pc);
        let mut plan = LinePlan {
            thread,
            mode,
            position,
            ranges,
            call: None,
            prologue_end: None,
            run_to: None,
            exits: None,
        };
        let result = if mode == Mode::Out {
            let frames = self.stack_trace(32)?;
            let mut physical = frames.iter().filter(|frame| !frame.is_inlined());
            let current = physical.next();
            let caller = physical.next();
            match (current.and_then(|frame| frame.return_address), caller) {
                (Some(address), Some(caller)) => self.run_line_to(&mut plan, address, caller.sp),
                _ => Err(DebuggerError::InvalidArgument(
                    "cannot recover the current frame's caller".into(),
                )),
            }
        } else {
            self.advance_line(&mut plan)
        };
        if let Err(error) = result {
            self.state = SessionState::Stopped;
            self.release_after_error(&mut plan);
            return Err(error);
        }
        self.line_plan = Some(plan);
        Ok(())
    }

    fn line_ranges(&self, pc: Address) -> Vec<Range<Address>>
    {
        self.symbols
            .image_for_address(pc)
            .and_then(|image| image.statement_at_address(pc).ok().flatten())
            .map_or_else(Vec::new, |line| line.ranges)
    }

    fn advance_line(&mut self, plan: &mut LinePlan) -> Result<()>
    {
        if plan.position.line.is_none() {
            return self.step_instruction(plan.thread);
        }
        let registers = self.backend.read_registers(plan.thread)?;
        let mut instruction = self.backend.read_memory(registers.pc, 4)?;
        self.overlay_originals(registers.pc, &mut instruction);
        if let Some(return_pc) = self.arch()?.call_return_address(registers.pc, &instruction) {
            if plan.mode == Mode::Over || plan.prologue_end.is_some() {
                return self.run_line_to(plan, return_pc, registers.sp);
            }
            plan.call = Some((return_pc, registers.sp));
        }
        self.step_instruction(plan.thread)
    }

    fn run_line_to(&mut self, plan: &mut LinePlan, address: Address, sp: Address) -> Result<()>
    {
        if let Some(event) = self.backend_wait(Some(std::time::Duration::ZERO))? {
            self.deferred = Some(event);
            self.state = SessionState::Running;
            self.last_stop = None;
            return Ok(());
        }
        let (id, owner) = self.take_site(address)?;
        plan.run_to = Some(RunTo { address, sp, id, owner });
        let arm_exits = plan.exits.is_none();
        if arm_exits {
            self.arm_exit_hooks(&mut plan.exits, plan.thread)?;
        }
        // start_continue also executes a restored instruction if the call itself is
        // at a user breakpoint. Its final request still holds every peer thread.
        let started =
            self.start_continue(ResumeRequest::all(ThreadAction::Suspend).with_thread(plan.thread, ThreadAction::Continue));
        match started {
            Err(error) if super::stepping::is_stop_pending(&error) => {
                // A stop arrived after the drain above and nothing ran: put the plan back
                // as that drain leaves it, and defer the stop the same way.
                if let Some(site) = plan.run_to.take() {
                    self.release_site(site.id, &site.owner)?;
                }
                if arm_exits {
                    self.release_exit_hooks(plan.exits.take())?;
                }
                self.deferred = Some(self.refused_stop()?);
            }
            started => started?,
        }
        self.state = SessionState::Running;
        self.last_stop = None;
        Ok(())
    }

    /// Release the return trap and the exit hooks. Every site is released even when one
    /// fails; the first failure is returned.
    fn release_line_site(&mut self, plan: &mut LinePlan) -> Result<()>
    {
        let mut result = Ok(());
        if let Some(site) = plan.run_to.take() {
            result = self.release_site(site.id, &site.owner);
        }
        let released = self.release_exit_hooks(plan.exits.take());
        if result.is_ok() {
            result = released;
        }
        result
    }

    /// One instruction step of a non-local exit completed. Once the thread leaves the
    /// exit's image above the stack it entered with, it has landed (at a landing pad, or
    /// where a `longjmp` goes): in the guarded frame or an outer one the step ends there,
    /// otherwise the call runs on.
    fn exit_stepped_in_line(&mut self, plan: &mut LinePlan) -> Result<Option<StopEvent>>
    {
        let thread = plan.thread;
        if self.interrupt_requested {
            return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
        }
        let (Some(exits), Some(site)) = (&mut plan.exits, &plan.run_to) else {
            return Ok(Some(StopEvent::SingleStep { thread }));
        };
        let guarded = site.sp;
        match self.exit_stepped(exits)? {
            ExitStep::Stepping => {
                self.step_instruction(thread)?;
                return Ok(None);
            }
            ExitStep::Landed { pc, sp } => {
                // A user breakpoint on the landing's first instruction is that breakpoint's
                // hit, as on any line-step stop; it must not be stepped over by the continue.
                if let Some(id) = self.breakpoints.id_for_kind(pc, BreakpointKind::Software)
                    && self.breakpoints.get(id).is_some_and(|entry| entry.info.enabled)
                {
                    self.release_line_site(plan)?;
                    return Ok(Some(StopEvent::Breakpoint { thread, pc }));
                }
                if sp >= guarded {
                    self.release_line_site(plan)?;
                    return Ok(Some(StopEvent::SingleStep { thread }));
                }
            }
            ExitStep::Lost => {}
        }
        let started =
            self.start_continue(ResumeRequest::all(ThreadAction::Suspend).with_thread(thread, ThreadAction::Continue));
        self.or_refused_stop(started)
    }

    /// Release the plan's temporary site after another error, keeping that error: a
    /// cleanup failure is logged rather than replacing it.
    fn release_after_error(&mut self, plan: &mut LinePlan)
    {
        if let Err(cleanup) = self.release_line_site(plan) {
            tracing::warn!("could not remove a temporary step breakpoint: {cleanup}");
        }
    }

    pub(super) fn cancel_line_step(&mut self) -> Result<()>
    {
        if let Some(mut plan) = self.line_plan.take() {
            self.state = SessionState::Stopped;
            self.release_line_site(&mut plan)?;
        }
        Ok(())
    }

    /// Called before Session counts breakpoint hits. Private rendezvous traps never
    /// become user hits, even if recursion reaches their address in a deeper frame.
    pub(super) fn finish_line_step(&mut self, event: StopEvent) -> Result<Option<StopEvent>>
    {
        let Some(mut plan) = self.line_plan.take() else {
            return Ok(Some(event));
        };
        if event.is_terminal() {
            return Ok(Some(event));
        }
        // Inspection uses the ordinary stopped-session APIs. The backend is already
        // stopped here; restore the public running state before returning to wait.
        self.state = SessionState::Stopped;
        let result = self.line_stopped(&mut plan, event);
        match result {
            Ok(None) => {
                self.line_plan = Some(plan);
                self.state = SessionState::Running;
                Ok(None)
            }
            Ok(Some(event)) => {
                self.release_line_site(&mut plan)?;
                self.state = SessionState::Running;
                Ok(Some(event))
            }
            Err(error) => {
                self.release_after_error(&mut plan);
                Err(error)
            }
        }
    }

    /// Whether `event` is a breakpoint stop left over from a private trap this session
    /// already removed (a return trap or a libunwind resume hook): an interrupt can stop
    /// the thread just as it reaches the trap, so the trap's exception is delivered
    /// later, when no trap is at that address any more (none in the table and no trap
    /// instruction in memory). With `thread`, only a stop of that thread qualifies.
    pub(super) fn stale_trap(&mut self, thread: Option<ThreadId>, event: &StopEvent) -> Result<bool>
    {
        let StopEvent::Breakpoint { thread: stopped, pc } = *event else {
            return Ok(false);
        };
        if thread.is_some_and(|thread| stopped != thread) {
            return Ok(false);
        }
        let arch = self.arch()?;
        let address = arch.pc_after_trap(pc);
        if self.breakpoints.id_for_kind(address, BreakpointKind::Software).is_some() {
            return Ok(false);
        }
        let trap = arch.trap_instruction();
        let memory = self.backend.read_memory(address, trap.len())?;
        Ok(memory != trap)
    }

    fn line_stopped(&mut self, plan: &mut LinePlan, event: StopEvent) -> Result<Option<StopEvent>>
    {
        let hook = match (&plan.run_to, &plan.exits) {
            (Some(_), Some(exits)) => self.exit_hook_hit(exits, &event)?,
            _ => None,
        };
        if let Some((thread, image)) = hook.filter(|&(thread, _)| thread == plan.thread) {
            if self.interrupt_requested {
                return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
            }
            if let Some(exits) = &mut plan.exits {
                self.begin_exit(exits, image)?;
            }
            self.step_instruction(thread)?;
            return Ok(None);
        }
        if plan.exits.as_ref().is_some_and(ExitHooks::leaving) && event == (StopEvent::SingleStep { thread: plan.thread }) {
            return self.exit_stepped_in_line(plan);
        }
        if let Some(site) = &plan.run_to {
            if let StopEvent::Breakpoint { thread, pc } = event
                && thread == plan.thread
                && self.arch()?.pc_after_trap(pc) == site.address
                && !matches!(site.owner, SiteOwner::User)
            {
                let registers = self.backend.read_registers(thread)?;
                if registers.sp != site.sp {
                    // An interrupt the backend dropped because this trap was queued
                    // ends the operation here; the caller releases the private site.
                    if self.interrupt_requested {
                        return Ok(Some(StopEvent::Interrupted { thread: Some(thread) }));
                    }
                    // The same instruction in a deeper recursive frame. Step off
                    // the private trap and continue waiting for the guarded frame.
                    let started = self.start_continue(
                        ResumeRequest::all(ThreadAction::Suspend).with_thread(thread, ThreadAction::Continue),
                    );
                    return self.or_refused_stop(started);
                }
                self.release_line_site(plan)?;
                if plan.mode == Mode::Out {
                    return Ok(Some(StopEvent::SingleStep { thread }));
                }
            } else if self.stale_trap(Some(plan.thread), &event)? {
                self.advance_line(plan)?;
                return Ok(None);
            } else {
                return Ok(Some(event));
            }
        } else if self.stale_trap(Some(plan.thread), &event)? {
            // The thread has not executed the instruction at the old trap's address yet.
            self.advance_line(plan)?;
            return Ok(None);
        } else if event != (StopEvent::SingleStep { thread: plan.thread }) {
            return Ok(Some(event));
        }
        if self.interrupt_requested {
            return Ok(Some(StopEvent::Interrupted {
                thread: Some(plan.thread),
            }));
        }
        let registers = self.backend.read_registers(plan.thread)?;
        // A single step can land directly on a user trap before executing it. Do
        // not let the next instruction step silently step over that user site.
        if let Some(id) = self.breakpoints.id_for_kind(registers.pc, BreakpointKind::Software)
            && self.breakpoints.get(id).is_some_and(|entry| entry.info.enabled)
        {
            return Ok(Some(StopEvent::Breakpoint {
                thread: plan.thread,
                pc: registers.pc,
            }));
        }
        if let Some((return_pc, sp)) = plan.call.take() {
            let position = self.position_at(registers.pc, None);
            let function = self
                .symbols
                .image_for_address(registers.pc)
                .and_then(|image| image.function_containing(registers.pc).ok().flatten());
            if let Some(function) = function.filter(|_| position.line.is_some()) {
                plan.prologue_end = Some(function.after_prologue);
            } else {
                self.run_line_to(plan, return_pc, sp)?;
                return Ok(None);
            }
        }
        if let Some(end) = plan.prologue_end {
            if registers.pc >= end {
                return Ok(Some(StopEvent::SingleStep { thread: plan.thread }));
            }
        } else if plan.position.line.is_none() {
            return Ok(Some(StopEvent::SingleStep { thread: plan.thread }));
        } else if !plan.ranges.iter().any(|range| range.contains(&registers.pc)) {
            let position = self.position_at(registers.pc, None);
            // Inlined helpers can have line rows in std while the physical source
            // position is still the original statement. Keep those within this step.
            if position.line.is_some() && position != plan.position {
                return Ok(Some(StopEvent::SingleStep { thread: plan.thread }));
            }
        }
        self.advance_line(plan)?;
        Ok(None)
    }
}
