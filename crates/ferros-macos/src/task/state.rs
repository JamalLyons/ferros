//! State checks, event delivery, and memory writes shared by the Backend impl.

#[allow(clippy::wildcard_imports)]
use super::*;

impl MacOSBackend
{
    pub(super) fn invalid_state(&self, what: &str) -> BackendError
    {
        let state = match self.state {
            BackendState::NoProcess => "there is no process",
            BackendState::Stopped => "the target is stopped",
            BackendState::Running => "the target is running",
        };
        BackendError::InvalidState(format!("{what} is not allowed while {state}"))
    }

    pub(super) fn require_stopped(&self, what: &str) -> BackendResult<()>
    {
        if self.state == BackendState::Stopped {
            Ok(())
        } else {
            Err(self.invalid_state(what))
        }
    }

    /// The port of `thread`, refreshing the thread list once if it is not known yet.
    pub(super) fn thread_port(&mut self, thread: ThreadId) -> BackendResult<thread_act_t>
    {
        let port = thread_act_t::try_from(thread.raw())
            .map_err(|_| BackendError::NotFound(format!("thread {} does not exist", thread.raw())))?;
        if !self.threads.contains(&port) {
            self.threads = threads::ThreadManager::refresh_thread_list(self.task, &self.threads).map_err(backend_error)?;
        }
        if self.threads.contains(&port) {
            Ok(port)
        } else {
            Err(BackendError::NotFound(format!(
                "thread {} is not part of process {}",
                thread.raw(),
                self.pid.0
            )))
        }
    }

    /// Record that `event` was returned by `wait` and update the state.
    ///
    /// A thread the event names that is not in the thread list yet (it started after the
    /// last enumeration) is added now, while the exception loop still holds the message's
    /// right to it: the backend's own right keeps the port name, which is the
    /// [`ThreadId`], valid after the loop releases its right on resume.
    ///
    /// A non-terminal stop ends the resume before it: the whole task is `task_suspend`ed,
    /// so the `thread_suspend`s of that resume are balanced now, and the single-step bit
    /// of every thread it asked to step is cleared (cancelling a step that did not
    /// complete).
    pub(super) fn deliver(&mut self, event: StopEvent) -> StopEvent
    {
        self.memory_cache.clear();
        if !event.is_terminal() {
            self.undo_thread_actions();
        }
        if let Some(thread) = event.thread()
            && let Ok(port) = thread_act_t::try_from(thread.raw())
            && !self.threads.contains(&port)
            && let Ok(threads) = threads::ThreadManager::refresh_thread_list(self.task, &self.threads)
        {
            self.threads = threads;
        }
        if event.is_terminal() {
            self.exit_watcher = None;
            let _ = self.stop_exception_handler(false);
            self.release_ports();
        } else {
            self.state = BackendState::Stopped;
        }
        event
    }

    /// Publish a batch the exception loop holds behind a failed `task_suspend` (#191),
    /// for a resume that must be refused because of it; returns that refusal.
    ///
    /// The target is stopped, so the backend already holds the task suspended (an
    /// interrupt whose `Interrupted` raced the batch, or the stop before it). One of
    /// those suspends is handed to the loop as an interrupt's would be
    /// ([`ExceptionSharedState::interrupt_suspend`]): the loop publishes the held batch
    /// with it as the stop, without a `task_suspend` of its own. The first event it
    /// publishes is peeked for the next `wait`. A suspend is taken here only if the
    /// backend holds none; its failure is returned instead of the refusal.
    pub(super) fn publish_held_batch(&mut self) -> BackendError
    {
        {
            let mut shared = lock_shared(&self.exception_state);
            if shared.held_unsuspended && !shared.interrupt_suspend {
                if shared.suspends == 0 {
                    // SAFETY: `self.task` is the send right this backend holds.
                    let kr = unsafe { task_suspend(self.task) };
                    if kr != KERN_SUCCESS {
                        return mach_error(kr, "task_suspend failed");
                    }
                    shared.suspends += 1;
                }
                shared.interrupt_suspend = true;
            }
        }
        exception::wake_loop(self.exception_port);
        if self.peeked.is_none()
            && let Some(event) = self
                .events_rx
                .as_ref()
                .and_then(|rx| rx.recv_timeout(EXCEPTION_REPLY_TIMEOUT).ok())
        {
            self.peeked = Some(event);
        }
        BackendError::StopPending
    }

    /// Take a queued event (or queued failure) without blocking.
    pub(super) fn try_next_event(&mut self) -> Option<LoopEvent>
    {
        self.peeked
            .take()
            .or_else(|| self.events_rx.as_ref().and_then(|rx| rx.try_recv().ok()))
    }

    /// Check `request` against what this backend supports.
    ///
    /// Every action is supported. A signal must name a thread holding a soft-signal
    /// exception that does not stay suspended (`Unsupported` otherwise), and a thread
    /// named in `thread_actions` must exist (`NotFound` otherwise).
    pub(super) fn check_resume_request(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        for (thread, signal) in &request.signals {
            if request.action_for(*thread) == ThreadAction::Suspend {
                return Err(BackendError::Unsupported(format!(
                    "a signal cannot be passed to thread {} while it stays suspended",
                    thread.raw()
                )));
            }
            if !lock_shared(&self.exception_state)
                .soft_signals
                .iter()
                .any(|&(port, _)| thread.raw() == u64::from(port))
            {
                return Err(BackendError::Unsupported(
                    "signal injection requires a held soft-signal exception".into(),
                ));
            }
            if !(0..32).contains(signal) {
                return Err(BackendError::Unsupported(format!("invalid signal {signal}")));
            }
        }
        for (thread, _) in &request.thread_actions {
            self.thread_port(*thread)?;
        }
        Ok(())
    }

    /// Write `data` at `address`, all or nothing (see [`write_memory_all_or_nothing`]).
    pub(super) fn write_target_memory(&mut self, address: Address, data: &[u8]) -> Result<()>
    {
        let result = if self.fail_memory_store.swap(false, Ordering::Relaxed) {
            Err(DebuggerError::Backend(BackendError::Os {
                code: -1,
                message: "injected memory store failure".into(),
            }))
        } else {
            write_memory_all_or_nothing(self.task, address, data)
        };
        self.memory_cache.invalidate_range(address, data.len());
        result
    }

    /// An engine write. A write over a birth trap updates the instruction that trap
    /// stands for and keeps the trap, but only once the target has accepted the bytes.
    ///
    /// The shared state stays locked from the plan to the commit, so the exception loop
    /// cannot arm, disarm or rewrite a birth site in between.
    pub(super) fn write_engine_memory(&mut self, address: Address, data: &[u8]) -> Result<()>
    {
        let shared = Arc::clone(&self.exception_state);
        let mut state = lock_shared(&shared);
        let Some(write) = state.birth.plan_engine_write(address.value(), data) else {
            drop(state);
            return self.write_target_memory(address, data);
        };
        self.write_target_memory(address, &write.bytes)?;
        state.birth.commit_engine_write(write);
        Ok(())
    }
}
