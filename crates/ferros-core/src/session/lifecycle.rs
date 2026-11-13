//! Process lifecycle: launch, attach, detach, resume and wait.

use std::time::{Duration, Instant};

use super::{ProcessOwnership, SIGKILL, Session, SessionState};
use crate::arch;
use crate::backend::{LaunchSpec, ResumeRequest, StopEvent};
use crate::error::{DebuggerError, Result};
use crate::symbols::SymbolCache;
use crate::types::ProcessId;

impl Session
{
    /// Typed error for `launch` and `attach` while a process exists.
    fn require_no_process(&self) -> Result<()>
    {
        match self.state {
            SessionState::NotStarted | SessionState::Exited => Ok(()),
            SessionState::Stopped | SessionState::Running => Err(DebuggerError::AlreadyAttached),
        }
    }

    /// Enter the initial stop of a new process.
    fn begin_process(&mut self, pid: ProcessId)
    {
        self.clear_process();
        // Images of an earlier process may sit at other addresses: start a fresh cache.
        self.symbols = SymbolCache::new();
        self.loaded_images.clear();
        self.pid = Some(pid);
        self.state = SessionState::Stopped;
        self.ownership = None;
        self.last_stop = None;
        self.sync_threads(None);
    }

    /// Start `spec.program`, stopped before its first instruction.
    ///
    /// Afterwards the session is [`SessionState::Stopped`], the first thread is
    /// selected and [`last_stop`](Self::last_stop) is `None`.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::AlreadyAttached`] while a process exists, or the backend's
    /// error (a [`BackendError::PermissionDenied`](crate::backend::BackendError::PermissionDenied)
    /// surfaces as [`DebuggerError::PermissionDenied`]).
    pub fn launch(&mut self, spec: &LaunchSpec) -> Result<ProcessId>
    {
        self.require_no_process()?;
        let pid = self.backend.launch(spec)?;
        self.begin_process(pid);
        self.ownership = Some(ProcessOwnership::Launched);
        Ok(pid)
    }

    /// Attach to the running process `pid` and stop it.
    ///
    /// Afterwards the session is [`SessionState::Stopped`], the first thread is
    /// selected and [`last_stop`](Self::last_stop) is `None`.
    ///
    /// # Errors
    ///
    /// As for [`launch`](Self::launch).
    pub fn attach(&mut self, pid: ProcessId) -> Result<()>
    {
        self.require_no_process()?;
        self.backend.attach(pid)?;
        self.begin_process(pid);
        self.ownership = Some(ProcessOwnership::Attached);
        Ok(())
    }

    /// Remove every inserted trap, then release the process, which keeps running.
    ///
    /// Each inserted software breakpoint gets its original bytes back first; the
    /// backend knows nothing about them. Afterwards the session is
    /// [`SessionState::NotStarted`] and the breakpoint table is empty.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error. If restoring a trap fails, the process is
    /// not detached: breakpoints already restored are left disabled, the others stay
    /// inserted, and the session stays stopped. If the backend's `detach` fails, every
    /// breakpoint is left disabled and the session stays stopped.
    pub fn detach(&mut self) -> Result<()>
    {
        self.require_stopped()?;
        self.restore_all_traps()?;
        self.backend.detach()?;
        self.clear_process();
        self.state = SessionState::NotStarted;
        self.ownership = None;
        self.last_stop = None;
        Ok(())
    }

    /// Continue every thread; the session becomes [`SessionState::Running`].
    ///
    /// As the backend contract requires, events still queued from the last stop (for
    /// example two threads that stopped at once) are drained first with a zero-timeout
    /// wait. If one is pending, the process is not resumed: the session still becomes
    /// running and the next [`wait`](Self::wait) returns that event.
    ///
    /// A signal held by the backend after a [`StopEvent::Signal`] stop is discarded: the
    /// program never sees it. Use [`resume_with_signal`](Self::resume_with_signal) to pass
    /// it on.
    ///
    /// Every thread whose pc is at an enabled software breakpoint is stepped over,
    /// one at a time in thread-list order, before continuing all threads. This also
    /// applies when the thread did not hit the breakpoint (for example a breakpoint
    /// added at its pc), matching GDB. Disabled and removed breakpoints are skipped.
    /// `resume` performs these steps itself, waiting at most
    /// [`STEP_OVER_TIMEOUT`](super::STEP_OVER_TIMEOUT) for each, and then continues, so
    /// the program runs whether or not the caller calls [`wait`](Self::wait). A step that
    /// does not complete in time (its thread is blocked) is left running and
    /// [`wait`](Self::wait) finishes the sequence. Internal step stops are never reported;
    /// any other event that arrives during a step is returned by the next `wait`, and so is
    /// a backend `wait` error. Only real breakpoint events increment hit counts. A passed
    /// signal goes on its thread's step if it steps over, otherwise on the final continue.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error (the session then stays stopped).
    /// A failed initial step start restores the removed trap before returning the
    /// error; if that restoration also fails, the breakpoint is disabled and its
    /// restoration error is returned. If a later step or the final continue fails to start
    /// after an internal step completed, the session is stopped (threads refreshed,
    /// [`last_stop`](Self::last_stop) `None`) and that error is returned.
    pub fn resume(&mut self) -> Result<()>
    {
        self.carried_signals.clear();
        self.resume_request(ResumeRequest::continue_all())
    }

    /// Like [`resume`](Self::resume), but when the last stop was
    /// [`StopEvent::Signal`] that signal is passed on to its thread
    /// (`ResumeRequest::continue_all().with_signal(thread, signal)`), so the program
    /// receives it. After any other stop it is the same as `resume`.
    ///
    /// If an earlier `resume_with_signal` passed a signal to a thread that never ran
    /// because another stop cut its step-over sequence short (the signal is still held),
    /// that signal is passed again now, together with the last stop's.
    ///
    /// # Errors
    ///
    /// As for [`resume`](Self::resume).
    pub fn resume_with_signal(&mut self) -> Result<()>
    {
        let mut request = ResumeRequest::continue_all();
        for (thread, signal) in std::mem::take(&mut self.carried_signals) {
            request = request.with_signal(thread, signal);
        }
        if let Some(StopEvent::Signal { thread, signal }) = self.last_stop.as_ref() {
            request = request.with_signal(*thread, *signal);
        }
        self.resume_request(request)
    }

    fn resume_request(&mut self, request: ResumeRequest) -> Result<()>
    {
        self.require_stopped()?;
        self.interrupt_requested = false;
        if let Some(event) = self.backend_wait(Some(Duration::ZERO))? {
            self.deferred = Some(event);
        } else {
            self.stale_step = None;
            self.start_continue(request)?;
        }
        self.state = SessionState::Running;
        self.last_stop = None;
        self.drive_step_overs()
    }

    /// Ask a running process to stop; returns immediately.
    ///
    /// The stop is reported by a later [`wait`](Self::wait), as
    /// [`StopEvent::Interrupted`] or as another event that raced it. While the process is
    /// stopped this does nothing and calls no backend method. The session state does not
    /// change until `wait` returns the event.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, or the backend's error (the
    /// state does not change).
    pub fn interrupt(&mut self) -> Result<()>
    {
        match self.state {
            SessionState::Stopped => Ok(()),
            SessionState::Running => {
                self.backend.interrupt()?;
                self.interrupt_requested = self.step_plan.is_some();
                Ok(())
            }
            SessionState::NotStarted | SessionState::Exited => Err(DebuggerError::NotAttached),
        }
    }

    /// Terminate the process, stopped or running, and return the event that records it.
    ///
    /// The backend reports no terminal event for a kill, so the session makes one:
    /// afterwards it is [`SessionState::Exited`], [`last_stop`](Self::last_stop) is
    /// `StopEvent::Killed { signal: SIGKILL }` (also the return value), and the pid,
    /// threads, selection and breakpoint table are cleared. Traps are not restored: the
    /// memory that held them is gone.
    ///
    /// If the process already ended while `resume` was stepping over a breakpoint (its
    /// terminal event is waiting for the next [`wait`](Self::wait)), nothing is killed:
    /// that event, for example `Exited { code }`, is recorded and returned instead.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, or the backend's error (the
    /// state and the breakpoint table do not change).
    pub fn kill(&mut self) -> Result<StopEvent>
    {
        self.require_process()?;
        // A terminal event `resume` already took from the backend means the process is
        // gone and the backend has nothing left to kill: report how it really ended.
        if let Some(event) = self.deferred.take_if(|event| event.is_terminal()) {
            self.clear_process();
            self.state = SessionState::Exited;
            self.ownership = None;
            self.last_stop = Some(event.clone());
            return Ok(event);
        }
        self.backend.kill()?;
        self.clear_process();
        self.state = SessionState::Exited;
        self.ownership = None;
        let event = StopEvent::Killed { signal: SIGKILL };
        self.last_stop = Some(event.clone());
        Ok(event)
    }

    /// Wait for the process to stop and return the event.
    ///
    /// - While running: `timeout = Some(d)` returns `Ok(None)` if nothing happened within
    ///   `d` (the session stays running); `None` blocks until an event arrives.
    /// - While stopped: returns an event still queued from the last stop, if any. With a
    ///   finite timeout `Ok(None)` means none is pending. With `timeout = None` the
    ///   backend is polled without blocking, and [`DebuggerError::NotRunning`] is returned
    ///   when nothing is pending, because the wait would never end.
    ///
    /// On an event the session becomes [`SessionState::Stopped`], or
    /// [`SessionState::Exited`] for a terminal event. A thread named by the event is
    /// selected. For [`StopEvent::Breakpoint`] the pc is mapped with
    /// [`Arch::pc_after_trap`](crate::arch::Arch::pc_after_trap), the returned event
    /// carries the mapped pc, and a hit is recorded on an enabled breakpoint at that
    /// address.
    ///
    /// Internal breakpoint step completions are consumed within this call. The timeout
    /// covers the whole sequence; a timeout leaves it running for the next `wait`.
    /// Other stops restore the trap and abandon the sequence; terminal events discard
    /// it without touching memory. If an interrupt races an internal completion, that
    /// completion becomes [`StopEvent::Interrupted`] and execution is not continued.
    /// If starting a later step or the final continue fails, the session is stopped,
    /// threads are refreshed, and `last_stop` is `None`. If restoring a trap fails,
    /// that breakpoint is left disabled and the same error-state rule applies.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotRunning`] as
    /// described above, or the backend's error. A failed backend wait leaves the state
    /// unchanged; failures after an internal stop follow the stopped-state rule above.
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<StopEvent>>
    {
        self.require_process()?;
        let started = Instant::now();
        let mut first = true;
        loop {
            let remaining = if first {
                timeout
            } else {
                timeout.map(|limit| limit.saturating_sub(started.elapsed()))
            };
            first = false;
            if self.state == SessionState::Running
                && let Some(error) = self.deferred_error.take()
            {
                return Err(error);
            }
            let event = match self.state {
                SessionState::Running => match self.deferred.take() {
                    Some(event) => Some(event),
                    None => self.backend_wait(remaining)?,
                },
                _ => match timeout {
                    Some(_) => self.backend_wait(remaining)?,
                    None => Some(self.backend_wait(Some(Duration::ZERO))?.ok_or(DebuggerError::NotRunning)?),
                },
            };
            let Some(event) = event else {
                return Ok(None);
            };
            match self.finish_step(event) {
                Ok(Some(event)) => return Ok(Some(self.handle_stop(event))),
                Ok(None) => {
                    self.interrupt_requested = false;
                    if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                        return Ok(None);
                    }
                }
                Err(error) => {
                    self.interrupt_requested = false;
                    self.state = SessionState::Stopped;
                    self.sync_threads(None);
                    self.last_stop = None;
                    return Err(error);
                }
            }
        }
    }

    /// Apply a stop event to the session and return it as the caller sees it.
    fn handle_stop(&mut self, event: StopEvent) -> StopEvent
    {
        self.interrupt_requested = false;
        if event.is_terminal() {
            self.clear_process();
            self.state = SessionState::Exited;
            self.last_stop = Some(event.clone());
            return event;
        }

        let event = match event {
            StopEvent::Breakpoint { thread, pc } => {
                let pc = arch::for_architecture(self.backend.architecture()).map_or(pc, |arch| arch.pc_after_trap(pc));
                self.breakpoints.record_hit(pc);
                StopEvent::Breakpoint { thread, pc }
            }
            other => other,
        };
        self.state = SessionState::Stopped;
        self.sync_threads(event.thread());
        self.last_stop = Some(event.clone());
        event
    }
}
