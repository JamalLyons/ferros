//! Per-thread resume actions and the newborn-thread (birth) policy.

#[allow(clippy::wildcard_imports)]
use super::*;

impl MacOSBackend
{
    /// Balance every `thread_suspend` of the last resume with `thread_resume`.
    ///
    /// Newborns held by the birth policy are included, each exactly once. A failure
    /// (the thread has exited, or the process is gone) is ignored.
    pub(super) fn release_thread_suspends(&mut self)
    {
        self.absorb_birth_holds();
        for port in std::mem::take(&mut self.suspended_threads) {
            // SAFETY: `port` is a thread right this backend holds (it is in `threads`);
            // the call only balances the suspend this backend added.
            let kr = unsafe { ffi::thread_resume(port) };
            if kr != KERN_SUCCESS {
                tracing::debug!("thread_resume({port}) failed with {kr}; the thread may have exited");
            }
        }
    }

    /// Clear the single-step bit of every thread the last resume asked to step.
    ///
    /// Clearing an already clear bit (the kernel clears it when the step completes) is
    /// harmless; a failure (the thread has exited) is ignored.
    pub(super) fn clear_step_bits(&mut self)
    {
        for port in std::mem::take(&mut self.stepping_threads) {
            if let Err(err) = crate::registers::set_single_step(port, false) {
                tracing::debug!("clearing the single-step bit of thread {port} failed: {err}");
            }
        }
    }

    /// The threads `request` suspends and steps, in that order.
    ///
    /// A request whose actions are all `Continue` names none and needs no thread list;
    /// otherwise the list is refreshed, so the default action covers every current thread.
    pub(super) fn plan_thread_actions(
        &mut self,
        request: &ResumeRequest,
    ) -> BackendResult<(Vec<thread_act_t>, Vec<thread_act_t>)>
    {
        let all_continue = request.default_action == ThreadAction::Continue
            && request
                .thread_actions
                .iter()
                .all(|(_, action)| *action == ThreadAction::Continue);
        if all_continue {
            return Ok((Vec::new(), Vec::new()));
        }
        self.threads = threads::ThreadManager::refresh_thread_list(self.task, &self.threads).map_err(backend_error)?;
        // `check_resume_request` checked the named threads against the cached list; a
        // thread that has exited since is gone from the refreshed one. Planning without it
        // would suspend every other thread with nothing left to step or run.
        if let Some((gone, _)) = request
            .thread_actions
            .iter()
            .find(|(thread, _)| !self.threads.iter().any(|&port| u64::from(port) == thread.raw()))
        {
            return Err(BackendError::NotFound(format!(
                "thread {} is not part of process {}",
                gone.raw(),
                self.pid.0
            )));
        }
        let mut suspend = Vec::new();
        let mut step = Vec::new();
        for &port in &self.threads {
            match request.action_for(ThreadId::from(u64::from(port))) {
                ThreadAction::Continue => {}
                ThreadAction::Suspend => suspend.push(port),
                ThreadAction::Step => step.push(port),
            }
        }
        Ok((suspend, step))
    }

    /// `thread_suspend` every thread in `suspend` and set the single-step bit of every
    /// thread in `step`, recording both. On failure everything done is undone and the
    /// error returned.
    pub(super) fn apply_thread_actions(&mut self, suspend: &[thread_act_t], step: &[thread_act_t]) -> BackendResult<()>
    {
        // Normally balanced already, when the stop that made the target *Stopped* was
        // delivered.
        self.release_thread_suspends();
        self.clear_step_bits();
        for &port in suspend {
            // SAFETY: `port` is a thread right this backend holds; the suspend is recorded
            // and balanced exactly once by `release_thread_suspends`.
            let kr = unsafe { ffi::thread_suspend(port) };
            if kr != KERN_SUCCESS {
                self.release_thread_suspends();
                return Err(mach_error(kr, &format!("thread_suspend of thread {port} failed")));
            }
            self.suspended_threads.push(port);
        }
        for &port in step {
            self.stepping_threads.push(port);
            if let Err(err) = crate::registers::set_single_step(port, true) {
                self.clear_step_bits();
                self.release_thread_suspends();
                return Err(backend_error(err));
            }
        }
        Ok(())
    }

    /// Undo [`apply_thread_actions`](Self::apply_thread_actions) (the rollback of a failed
    /// resume, and teardown). Single-step bits are cleared before any `thread_resume`,
    /// so a thread let go on detach or failure cannot complete a stale step.
    pub(super) fn undo_thread_actions(&mut self)
    {
        self.clear_step_bits();
        self.clear_birth_step_offs();
        self.release_thread_suspends();
    }

    /// Move birth holds onto [`Self::suspended_threads`], keeping one send right each.
    pub(super) fn absorb_birth_holds(&mut self)
    {
        let holds = std::mem::take(&mut lock_shared(&self.exception_state).birth.suspended);
        for port in holds {
            if self.threads.contains(&port) {
                // SAFETY: the birth record and `threads` each own a send right of this name.
                unsafe {
                    let _ = ffi::mach_port_deallocate(mach_task_self(), port);
                }
            } else {
                self.threads.push(port);
            }
            if !self.suspended_threads.contains(&port) {
                self.suspended_threads.push(port);
            }
        }
    }

    /// Clear hardware steps that were only leaving a birth trap, and release threads
    /// frozen for that step.
    pub(super) fn clear_birth_step_offs(&mut self)
    {
        for port in lock_shared(&self.exception_state).birth.clear_step_offs() {
            if let Err(err) = crate::registers::set_single_step(port, false) {
                tracing::debug!("clearing a birth step-off bit of thread {port} failed: {err}");
            }
        }
        if let Err(err) = self.thaw_birth_freeze() {
            tracing::error!("{err}");
        }
    }

    /// Balance `thread_suspend`s taken so other threads cannot pass a disarmed birth site.
    ///
    /// A thread that is already dead is dropped. Any other failure puts the port back
    /// so a later thaw can retry it.
    pub(super) fn thaw_birth_freeze(&mut self) -> std::result::Result<(), String>
    {
        let ports = lock_shared(&self.exception_state).birth.take_frozen();
        let mut resumed = Vec::new();
        let mut failed = Vec::new();
        for port in ports {
            // SAFETY: `port` was suspended by `prepare_birth_step_off` and is a thread
            // right this backend holds.
            let kr = unsafe { ffi::thread_resume(port) };
            if kr == mach2::kern_return::KERN_SUCCESS
                || kr == mach2::kern_return::KERN_INVALID_NAME
                || kr == mach2::kern_return::KERN_TERMINATED
            {
                resumed.push(port);
            } else {
                failed.push(port);
            }
        }
        let owned = lock_shared(&self.exception_state).birth.take_owned(&resumed);
        for port in owned {
            // SAFETY: this drops the extra send right taken when the exception loop
            // froze the thread. A dead name has already lost that right.
            unsafe {
                let _ = ffi::mach_port_deallocate(mach_task_self(), port);
            }
        }
        if failed.is_empty() {
            return Ok(());
        }
        let mut shared = lock_shared(&self.exception_state);
        for port in failed {
            shared.birth.freeze(port);
        }
        Err("thread_resume of a birth step-off freeze failed".into())
    }

    /// A continue can map `libsystem_pthread` in this resume. A step cannot.
    pub(super) fn birth_cover_required(&self, request: &ResumeRequest) -> bool
    {
        self.threads
            .iter()
            .any(|port| request.action_for(ThreadId::from(u64::from(*port))) == ThreadAction::Continue)
    }

    /// Arm or remove new-thread traps to match `request`, before any thread runs.
    pub(super) fn sync_birth_policy(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        if request.default_action == ThreadAction::Continue {
            return self.disarm_birth();
        }
        // Threads told to `Continue` are not newborns, even when one re-enters a pthread
        // entry as a reused workqueue thread.
        let running = self
            .threads
            .iter()
            .copied()
            .filter(|port| request.action_for(ThreadId::from(u64::from(*port))) == ThreadAction::Continue)
            .collect();
        lock_shared(&self.exception_state).birth.set_running(running);
        let holds_new_threads = request.default_action == ThreadAction::Suspend
            && request
                .thread_actions
                .iter()
                .any(|(_, action)| *action != ThreadAction::Suspend);
        if !holds_new_threads {
            return Ok(());
        }
        if self.architecture != Architecture::Arm64 {
            return Err(BackendError::Unsupported(
                "suspending threads created during a resume is supported for arm64 targets only".into(),
            ));
        }
        {
            let mut shared = lock_shared(&self.exception_state);
            match shared.birth.arm(self.task, self.pid, self.birth_cover_required(request)) {
                Ok(()) => {}
                Err(crate::birth::BirthInstallError::Unsupported(message)) => {
                    return Err(BackendError::Unsupported(message));
                }
                Err(crate::birth::BirthInstallError::Os(message)) => {
                    return Err(BackendError::Os { code: -1, message });
                }
            }
        }
        // A previous step-off may have kept peers frozen after a failed restore.
        // The trap is armed again now (or there is nothing to protect), so those
        // suspends must not outlive this resume.
        self.thaw_birth_freeze()
            .map_err(|message| BackendError::Os { code: -1, message })?;
        self.memory_cache.clear();
        Ok(())
    }

    pub(super) fn disarm_birth(&mut self) -> BackendResult<()>
    {
        let result = lock_shared(&self.exception_state)
            .birth
            .disarm(self.task)
            .map_err(|message| BackendError::Os { code: -1, message });
        if result.is_ok() {
            // Original instructions are back, so a freeze from a step-off no longer
            // protects an open entry.
            if let Err(err) = self.thaw_birth_freeze() {
                self.memory_cache.clear();
                return Err(BackendError::Os { code: -1, message: err });
            }
        }
        self.memory_cache.clear();
        result
    }

    pub(super) fn rearm_birth_if_active(&mut self)
    {
        lock_shared(&self.exception_state).birth.rearm_all(self.task);
        self.memory_cache.clear();
    }

    /// A runnable thread parked on an armed birth trap must execute the original
    /// instruction. With the trap still required, that is one hardware step.
    ///
    /// The restored instruction is process-wide. Every other thread is
    /// `thread_suspend`ed until the trap is written back, so a creator running in
    /// the same resume cannot start another thread through the open entry.
    pub(super) fn prepare_birth_step_off(&mut self, suspend: &[thread_act_t]) -> BackendResult<()>
    {
        if !lock_shared(&self.exception_state).birth.is_active() {
            return Ok(());
        }
        let runnable: Vec<thread_act_t> = self.threads.iter().copied().filter(|port| !suspend.contains(port)).collect();
        let mut leaving = Vec::new();
        for port in runnable {
            let pc = read_registers_for_port(self.architecture, port)
                .map_err(backend_error)?
                .pc
                .value();
            if !lock_shared(&self.exception_state).birth.armed_addresses().contains(&pc) {
                continue;
            }
            let user_step = self.stepping_threads.contains(&port);
            lock_shared(&self.exception_state)
                .birth
                .disarm_address(self.task, pc)
                .map_err(|message| BackendError::Os { code: -1, message })?;
            if !user_step {
                lock_shared(&self.exception_state).birth.note_step_off(port);
                crate::registers::set_single_step(port, true).map_err(backend_error)?;
            }
            leaving.push(port);
        }
        if !leaving.is_empty() {
            let mut frozen = Vec::new();
            for &port in &self.threads {
                if suspend.contains(&port) || leaving.contains(&port) {
                    continue;
                }
                // SAFETY: `port` is a thread right this backend holds. The suspend is
                // recorded on the birth policy and balanced by `thaw_birth_freeze`.
                let kr = unsafe { ffi::thread_suspend(port) };
                if kr != KERN_SUCCESS {
                    for port in frozen {
                        // SAFETY: this balances the suspend just taken above.
                        let _ = unsafe { ffi::thread_resume(port) };
                    }
                    return Err(mach_error(kr, &format!("thread_suspend of thread {port} failed")));
                }
                frozen.push(port);
            }
            let mut shared = lock_shared(&self.exception_state);
            for port in frozen {
                shared.birth.freeze(port);
            }
        }
        self.memory_cache.clear();
        Ok(())
    }
}
