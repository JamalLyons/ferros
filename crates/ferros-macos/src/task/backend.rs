//! The [`Backend`] implementation and teardown on drop.

#[allow(clippy::wildcard_imports)]
use super::*;

impl Backend for MacOSBackend
{
    /// Launch `spec.program` with `posix_spawn()` and `POSIX_SPAWN_START_SUSPENDED`, then
    /// take its task port.
    ///
    /// `spec.program` is argv\[0\], followed by `spec.args`; the environment, working
    /// directory and standard streams follow `spec`. If the task port cannot be obtained,
    /// the spawned process is killed and reaped before the error is returned.
    fn launch(&mut self, spec: &LaunchSpec) -> BackendResult<ProcessId>
    {
        use tracing::{debug, info};

        if self.state != BackendState::NoProcess {
            return Err(self.invalid_state("launch"));
        }

        let pid = launch::LaunchManager::launch(spec).map_err(backend_error)?;
        debug!("Attaching to spawned process");
        let process_id = ProcessId::from(pid as u32);
        if let Err(err) = self.attach_task(process_id) {
            Self::discard_child(pid).map_err(|cleanup| BackendError::Os {
                code: -1,
                message: format!("launch failed ({err}); child cleanup also failed: {cleanup}"),
            })?;
            return Err(backend_error(err));
        }
        self.launched = true;
        self.child_cleanup.launched(pid);
        self.state = BackendState::Stopped;
        if let Err(err) = self.start_tracing() {
            self.kill().map_err(|cleanup| BackendError::Os {
                code: -1,
                message: format!("tracing setup failed ({err}); child cleanup also failed: {cleanup}"),
            })?;
            return Err(err);
        }
        info!("Successfully launched and attached to process {}", pid);
        Ok(process_id)
    }

    /// Attach with `task_for_pid()` and PT_ATTACHEXC. Its private SIGSTOP
    /// establishes the initial whole-task suspension and is suppressed internally.
    fn attach(&mut self, pid: ProcessId) -> BackendResult<()>
    {
        if self.state != BackendState::NoProcess {
            return Err(self.invalid_state("attach"));
        }

        self.attach_task(pid).map_err(backend_error)?;
        self.state = BackendState::Stopped;
        if let Err(err) = self.start_tracing() {
            // Before tracing succeeds no SIGSTOP was sent; afterwards undo tracing
            // before restoring ports, so no signal can strand the target.
            if self.traced {
                let _ = self.detach_target();
            } else {
                let _ = self.stop_exception_handler(true);
                self.release_suspends();
                self.release_ports();
            }
            return Err(err);
        }
        Ok(())
    }

    /// Restore the target's exception ports, shut down the exception handler (replying to
    /// held and queued exceptions), balance the task suspend count with `task_resume`, and
    /// release the ports. The target keeps running.
    fn detach(&mut self) -> BackendResult<()>
    {
        use tracing::info;

        self.require_stopped("detach")?;
        let alive = !lock_shared(&self.exception_state).dead && !crate::lifecycle::task_is_dead(self.task);
        if !alive {
            return Err(BackendError::NotFound(format!("process {} has exited", self.pid.0)));
        }

        let pid = self.pid.0;
        info!("Detaching from process {}", pid);
        self.detach_target()?;
        info!("Successfully detached from process {}", pid);
        Ok(())
    }

    fn hold_attached_on_drop(&mut self)
    {
        {
            let mut shared = lock_shared(&self.exception_state);
            if shared.dead || crate::lifecycle::task_is_dead(self.task) {
                return;
            }
            if shared.suspends == 0 {
                let kr = if self.fail_hold_suspend.swap(false, Ordering::Relaxed) {
                    mach2::kern_return::KERN_FAILURE
                } else {
                    // SAFETY: this backend still owns the target task right. A concurrent
                    // exception stop takes the same lock before acquiring its suspension.
                    unsafe { task_suspend(self.task) }
                };
                if kr == KERN_SUCCESS {
                    shared.suspends += 1;
                } else {
                    tracing::error!(
                        "could not suspend retained target {}: {}",
                        self.pid.0,
                        mach_error(kr, "task_suspend failed")
                    );
                }
            }
        }

        // Keep the exception receive right and exit watcher alive. Normal backend Drop
        // can clean up once the target dies. If that cannot be observed, retain the
        // resources until debugger exit to avoid the ordinary detach path.
        let retained = ManuallyDrop::new(std::mem::replace(self, Self::empty()));
        if let Err(err) = thread::Builder::new().name("ferros-retained-target".into()).spawn(move || {
            let waited = retained.exit_watcher.as_ref().map(|watcher| watcher.wait(Duration::MAX));
            let dead = lock_shared(&retained.exception_state).dead || crate::lifecycle::task_is_dead(retained.task);
            if dead {
                drop(ManuallyDrop::into_inner(retained));
            } else {
                tracing::error!("could not wait for retained target {} to exit: {waited:?}", retained.pid.0);
            }
        }) {
            tracing::error!("could not start cleanup for retained target: {err}");
        }
    }

    /// SIGKILL works at Mach and ptrace stops; release holds and await the sole reaper.
    fn kill(&mut self) -> BackendResult<()>
    {
        if self.state == BackendState::NoProcess {
            return Err(self.invalid_state("kill"));
        }

        let pid = self.pid.0 as libc::pid_t;
        {
            let shared = lock_shared(&self.exception_state);
            // The watcher marks death before it can reap, under this lock. Never
            // signal a remembered pid once reaping may have allowed pid reuse.
            if !shared.dead {
                // SAFETY: this tracee is not yet reaped; kill does not access memory.
                let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
                if killed != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::ESRCH) {
                        return Err(backend_error(DebuggerError::Io(err)));
                    }
                }
            }
        }

        // The process is dying: its exception ports need no restoring, only releasing.
        // Threads kept suspended by a resume in flight are let go too, so nothing of
        // ours holds a thread back from exiting. Birth traps are removed first so a
        // newborn is not released onto a `brk`.
        let _ = self.disarm_birth();
        self.undo_thread_actions();
        let _ = self.stop_exception_handler(false);
        self.release_suspends();
        let reaped = if let Some(watcher) = &self.exit_watcher {
            watcher.wait(KILL_REAP_TIMEOUT)
        } else if self.launched {
            let result = launch::reap_child(pid, KILL_REAP_TIMEOUT);
            if result.is_ok() {
                self.child_cleanup.reaped();
            }
            result
        } else {
            Ok(())
        };
        self.exit_watcher = None;
        if reaped.is_err() && self.launched {
            // A failed watcher is no longer a reaper. Transfer the remaining child
            // claim before local teardown, so even a failed fallback remains owned
            // by the harness/embedding cleanup handle rather than being forgotten.
            self.child_cleanup.detach(self.traced);
            let _ = self.child_cleanup.reap();
        }
        self.release_ports();
        reaped.map_err(|err| BackendError::KillCleanup(err.to_string()))
    }

    /// Receive the next stop event from the exception loop (or an interrupt).
    ///
    /// An exception reply failure is returned once as `BackendError::Os` (the state
    /// does not change), and later calls report later stops as usual.
    ///
    /// The kqueue watcher publishes terminal events after waitpid has collected status.
    fn wait(&mut self, timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>
    {
        if self.state == BackendState::NoProcess {
            return Err(self.invalid_state("wait"));
        }
        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            let queued = self.try_next_event();
            let shared = lock_shared(&self.exception_state);
            let dead = shared.dead || crate::lifecycle::task_is_dead(self.task);
            // The watcher has ended: after draining queued terminal events, neither
            // this nor a later wait(None) may block waiting for it to send another.
            if queued.is_none()
                && let Some(error) = &shared.watcher_error
            {
                return Err(error.clone());
            }
            drop(shared);
            if queued.is_none() && self.state == BackendState::Stopped && timeout.is_none() && !dead {
                return Err(BackendError::InvalidState(
                    "wait without a timeout while stopped with no pending event would block forever".into(),
                ));
            }
            let received = match queued {
                Some(event) => Ok(event),
                None => {
                    let rx = self.events_rx.as_ref().ok_or_else(|| BackendError::Os {
                        code: -1,
                        message: "the exception handler is not running".into(),
                    })?;
                    match deadline {
                        None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    }
                }
            };
            match received {
                Ok(Ok(event)) if event.is_terminal() => return Ok(Some(self.deliver(event))),
                // A terminal event takes precedence over stale stops and reply failures
                // from dead threads, even if those reached the channel first.
                Ok(_) if dead || crate::lifecycle::task_is_dead(self.task) => continue,
                Ok(Ok(event)) => return Ok(Some(self.deliver(event))),
                Ok(Err(err)) => return Err(err),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(BackendError::Os {
                        code: -1,
                        message: "the event channel stopped".into(),
                    });
                }
            }
        }
    }

    /// Resume the target as `request` describes, in this order:
    ///
    /// 1. `thread_suspend` every thread whose action is `Suspend` and set the single-step
    ///    bit (`MDSCR_EL1.SS`) of every thread whose action is `Step`;
    /// 2. release runnable threads' stop records and `task_resume` once per suspend the
    ///    backend holds. When forwarding signals, hold their threads suspended and keep
    ///    their stop records until `task_resume` succeeds, then select the signals before
    ///    letting those threads run. Selectively suspended threads keep their records.
    ///
    /// A step that completes is reported by `wait` as [`StopEvent::SingleStep`]. Whatever
    /// stop `wait` reports next, the `thread_suspend`s are then balanced and every
    /// single-step bit set here is cleared (see `deliver`).
    ///
    /// The next `wait` reports failed replies and signal selection failures after the
    /// task resumes. If a step before that fails, what was done is undone (`thread_resume`,
    /// single-step bits cleared, the suspend count restored) and the target stays *Stopped*.
    /// A reply error already queued when `task_resume` fails is still reported once by
    /// `wait` (or the session's next resume).
    ///
    /// When forwarding signals, `task_resume` comes before signal selection and the
    /// exception loop's Continue, so their failures cannot be undone: the resume returns
    /// `Ok`, the target is *Running*, and the next `wait` reports each failure once. A
    /// Continue the loop does not acknowledge within `EXCEPTION_REPLY_TIMEOUT` (5 s) is
    /// such a failure: the resume blocks for that long, then returns `Ok` with the target
    /// running. The command stays queued, so the loop still answers the records of
    /// runnable threads when it takes it; until then those threads may stay at their
    /// exceptions (#250 W9).
    ///
    /// If `task_resume` fails partway and the suspends already balanced cannot be put
    /// back, the task may run: the resume returns `Ok`, the target is *Running* (still
    /// suspended if the backend holds suspends; `interrupt` then stops it), and the next
    /// `wait` reports the error (#250 W11).
    ///
    /// Held Unix signals are suppressed unless named in `request.signals`. A signal named
    /// for a thread whose action is `Suspend`, or for a thread without a held soft-signal
    /// exception, returns [`BackendError::Unsupported`]; so does `Step` on x86-64.
    ///
    /// A stop the exception loop received but could not publish because its own
    /// `task_suspend` failed (#191) is a pending stop too: the resume is refused with
    /// [`BackendError::StopPending`] after that stop is published (see
    /// [`publish_held_batch`](Self::publish_held_batch)), so the next `wait` returns it
    /// rather than reporting it after this resume, which it does not belong to.
    fn resume(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        use tracing::info;

        self.require_stopped("resume")?;
        if lock_shared(&self.exception_state).dead || crate::lifecycle::task_is_dead(self.task) {
            return Err(BackendError::InvalidState(
                "process death is pending; drain it with wait".into(),
            ));
        }
        self.check_resume_request(request)?;
        let (suspend, step) = self.plan_thread_actions(request)?;
        // The exception loop queues a stop event under this lock, together with its
        // suspend and the held-exception flag. Checking the queue under the same lock
        // means a stop is either fully visible here (and the resume is refused) or has
        // not happened yet.
        let shared_state = Arc::clone(&self.exception_state);
        let (count, pending, held, signal_threads) = {
            let mut shared = lock_shared(&shared_state);
            if let Some(event) = self.try_next_event() {
                self.peeked = Some(event);
                return Err(BackendError::StopPending);
            }
            if shared.held_unsuspended || shared.interrupt_suspend {
                drop(shared);
                return Err(self.publish_held_batch());
            }
            let mut signal_threads: Vec<_> = shared
                .soft_signals
                .iter()
                .map(|&(port, _)| port)
                .filter(|&port| {
                    request
                        .signal_for(ThreadId::from(u64::from(port)))
                        .is_some_and(|signal| signal != 0)
                })
                .collect();
            signal_threads.sort_unstable();
            signal_threads.dedup();
            shared.preparing_resume = true;
            (
                std::mem::take(&mut shared.suspends),
                shared.pending_exception,
                !shared.held.is_empty(),
                signal_threads,
            )
        };
        self.reply_faults.publish_resume_race();
        let restore = |backend: &mut Self| {
            backend.undo_thread_actions();
            backend.rearm_birth_if_active();
            let mut shared = lock_shared(&backend.exception_state);
            shared.suspends += count;
            shared.preparing_resume = false;
        };
        let resume_failed = |backend: &mut Self, err| {
            // The task can become inactive before its port becomes a dead name or
            // NOTE_EXIT is published. Give the watcher a bounded chance to confirm.
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                if lock_shared(&backend.exception_state).dead || crate::lifecycle::task_is_dead(backend.task) {
                    backend.state = BackendState::Running;
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            restore(backend);
            Err(err)
        };
        let events = self.events_tx.clone();
        let report_error = |err| {
            if let Some(events) = &events {
                let _ = events.send(Err(err));
            }
        };
        let forwarding = !signal_threads.is_empty();

        // Traps go in before any thread can run. A failure here leaves the snapshot
        // policy unused: the resume returns and the target stays stopped.
        if let Err(err) = self.sync_birth_policy(request) {
            restore(self);
            return Err(err);
        }

        // Suspend and arm the threads before any of them can run.
        if let Err(err) = self.apply_thread_actions(&suspend, &step) {
            restore(self);
            return Err(err);
        }
        if forwarding && let Err(err) = self.prepare_birth_step_off(&suspend) {
            restore(self);
            return Err(err);
        }
        self.memory_cache.clear();

        {
            // G8 (#57): the task remains physically suspended, but the exception
            // loop may already have received a request before the interrupt that
            // stopped us. It can publish that request between our lock sections.
            // Recheck for its stop before changing signal decisions, and hold this
            // same lock through direct replies: otherwise those replies could erase
            // the new stop's records while the loop waits for its Continue command.
            let mut shared = lock_shared(&shared_state);
            if let Some(event) = self.try_next_event() {
                self.peeked = Some(event);
                drop(shared);
                restore(self);
                return Err(BackendError::StopPending);
            }
            if shared.held_unsuspended {
                drop(shared);
                restore(self);
                return Err(self.publish_held_batch());
            }
            // A thread receiving a signal must stay at its stop until the signal is
            // selected. Its held record keeps the thread right alive until Continue.
            let signal_suspends = signal_threads
                .into_iter()
                .map(ThreadSuspendGuard::new)
                .collect::<Result<Vec<_>>>();
            let signal_suspends = match signal_suspends {
                Ok(suspends) => suspends,
                Err(err) => {
                    drop(shared);
                    restore(self);
                    return Err(backend_error(err));
                }
            };
            if forwarding {
                // Once these suspends are balanced, the loop must not use them to
                // answer deferred exceptions, even if Continue has not arrived yet.
                shared.preparing_resume = false;
                match self.resume_task(count) {
                    Ok(()) => {}
                    Err(ResumeTaskError::Restored(err)) => {
                        shared.preparing_resume = true;
                        drop(shared);
                        return resume_failed(self, err);
                    }
                    Err(ResumeTaskError::Partial { error, held }) => {
                        shared.suspends += held;
                        report_error(error);
                    }
                }
            }
            if pending || held {
                // A selectively suspended thread retains its logical signal decision
                // until a later resume allows that thread to run.
                let signals = shared.soft_signals.clone();
                for (port, original) in signals.into_iter().filter(|(port, _)| !suspend.contains(port)) {
                    let signal = request.signal_for(ThreadId::from(u64::from(port))).unwrap_or(0);
                    if let Err(err) = self.select_resumed_signal(&mut shared, port, original, signal) {
                        if forwarding {
                            report_error(err);
                        } else {
                            drop(shared);
                            restore(self);
                            return Err(err);
                        }
                    }
                }
            }
            drop(signal_suspends);
            if !pending
                && held
                && let Some(events) = &events
            {
                // Only retained stop records remain; the loop is receiving rather
                // than awaiting Continue, so their replies are owned here.
                exception::answer_held_exceptions(&mut shared, &suspend, &self.reply_faults, events, self.task);
            }
        }
        if pending {
            let (ack_tx, ack_rx) = mpsc::channel();
            let keep = suspend.clone();
            let sent = self.exception_commands.as_ref().is_some_and(|tx| {
                tx.send(ExceptionLoopCommand::Continue {
                    ack: ack_tx,
                    keep,
                    discard_deferred: false,
                })
                .is_ok()
            });
            if sent {
                exception::wake_loop(self.exception_port);
            }
            let replied = if sent {
                ack_rx
                    .recv_timeout(EXCEPTION_REPLY_TIMEOUT)
                    .map_err(|_| "the exception handler did not answer")
            } else {
                Err("the exception handler is not running")
            };
            if let Err(message) = replied {
                let err = BackendError::Os {
                    code: -1,
                    message: format!("failed to reply to the Mach exception: {message}"),
                };
                if forwarding {
                    report_error(err);
                } else {
                    restore(self);
                    return Err(err);
                }
            }
        }

        if !forwarding {
            if let Err(err) = self.prepare_birth_step_off(&suspend) {
                restore(self);
                return Err(err);
            }
            // The loop can no longer use these in-flight suspends to answer deferred signals.
            lock_shared(&self.exception_state).preparing_resume = false;
            match self.resume_task(count) {
                Ok(()) => {}
                Err(ResumeTaskError::Restored(err)) => return resume_failed(self, err),
                Err(ResumeTaskError::Partial { error, held }) => {
                    lock_shared(&self.exception_state).suspends += held;
                    report_error(error);
                }
            }
        }
        self.state = BackendState::Running;
        info!("Resumed process {}", self.pid.0);
        Ok(())
    }

    /// Stop a running target with `task_suspend()` and queue `Interrupted`.
    ///
    /// While the exception loop holds a batch whose own `task_suspend` failed (#191),
    /// the interrupt's suspend is handed to the loop instead, which publishes that held
    /// batch as the stop: a completed step or a breakpoint reported after a resume
    /// would otherwise belong to a resume it did not come from.
    fn interrupt(&mut self) -> BackendResult<()>
    {
        match self.state {
            BackendState::NoProcess => Err(self.invalid_state("interrupt")),
            BackendState::Stopped => Ok(()),
            BackendState::Running => {
                {
                    let mut shared = lock_shared(&self.exception_state);
                    // An exception stop is already queued for `wait`, or a signal was
                    // deferred because an earlier interrupt holds the task: the interrupt
                    // lost the race and is consumed, as the contract requires.
                    // So is one already handed to a held batch.
                    if shared.pending_exception || shared.interrupt_suspend {
                        return Ok(());
                    }
                    if self.drop_next_interrupt.swap(false, Ordering::Relaxed) {
                        return Ok(());
                    }
                    let kr = unsafe { task_suspend(self.task) };
                    if kr != KERN_SUCCESS {
                        return Err(mach_error(kr, "task_suspend failed"));
                    }
                    shared.suspends += 1;
                    if shared.held_unsuspended {
                        shared.interrupt_suspend = true;
                        drop(shared);
                        exception::wake_loop(self.exception_port);
                        return Ok(());
                    }
                    if let Some(tx) = &self.events_tx {
                        let _ = tx.send(Ok(StopEvent::Interrupted { thread: None }));
                    }
                }
                Ok(())
            }
        }
    }

    /// Enumerate the threads from the OS; a thread's id is its Mach thread port name.
    fn threads(&mut self) -> BackendResult<Vec<ThreadId>>
    {
        self.require_stopped("threads")?;
        self.threads = threads::ThreadManager::refresh_thread_list(self.task, &self.threads).map_err(backend_error)?;
        if self.reply_faults.sorted_threads.load(std::sync::atomic::Ordering::SeqCst) {
            self.threads.sort_unstable();
        }
        Ok(self.threads.iter().map(|&port| ThreadId::from(u64::from(port))).collect())
    }

    fn read_registers(&mut self, thread: ThreadId) -> BackendResult<Registers>
    {
        self.require_stopped("read_registers")?;
        let port = self.thread_port(thread)?;
        read_registers_for_port(self.architecture, port).map_err(backend_error)
    }

    fn write_registers(&mut self, thread: ThreadId, registers: &Registers) -> BackendResult<()>
    {
        self.require_stopped("write_registers")?;
        if registers.architecture() != self.architecture {
            return Err(BackendError::Unsupported(format!(
                "registers for {} cannot be written to a {} target",
                registers.architecture(),
                self.architecture
            )));
        }
        let port = self.thread_port(thread)?;
        write_registers_to_port(self.architecture, port, registers).map_err(backend_error)
    }

    fn read_memory(&mut self, address: Address, len: usize) -> BackendResult<Vec<u8>>
    {
        self.require_stopped("read_memory")?;
        let mut bytes = self.memory_cache.read(self.task, address, len).map_err(backend_error)?;
        if bytes.len() != len {
            return Err(BackendError::NotFound(format!(
                "only {} of {len} bytes at 0x{:016x} are mapped",
                bytes.len(),
                address.value()
            )));
        }
        // The birth traps are the backend's own; the engine sees the instructions under them.
        lock_shared(&self.exception_state)
            .birth
            .mask_read(address.value(), &mut bytes);
        Ok(bytes)
    }

    fn write_memory(&mut self, address: Address, data: &[u8]) -> BackendResult<()>
    {
        self.require_stopped("write_memory")?;
        if self.fail_memory_write.swap(false, Ordering::Relaxed) {
            return Err(BackendError::Os {
                code: -1,
                message: "injected memory write failure".into(),
            });
        }
        self.write_engine_memory(address, data).map_err(backend_error)
    }

    fn memory_regions(&mut self) -> BackendResult<Vec<MemoryRegion>>
    {
        self.require_stopped("memory_regions")?;
        get_memory_regions(self.task).map_err(backend_error)
    }

    /// The executable and libraries reported by the target loader (see [`crate::images`]).
    fn images(&mut self) -> BackendResult<Vec<LoadedImage>>
    {
        self.require_stopped("images")?;
        let lookup = images::current_uuid_lookup();
        if lookup == images::UuidLookup::Cache {
            images::sync_test_epoch(&mut self.mapped_uuids);
        }
        images::loaded_images_with(self.task, self.pid, &mut self.mapped_uuids, lookup).map_err(backend_error)
    }

    fn architecture(&self) -> Architecture
    {
        self.architecture
    }
}

impl Drop for MacOSBackend
{
    fn drop(&mut self)
    {
        if self.state == BackendState::NoProcess {
            self.exit_watcher = None;
            let _ = self.stop_exception_handler(false);
            self.release_ports();
            return;
        }
        if self.launched {
            let _ = Backend::kill(self);
        } else if lock_shared(&self.exception_state).dead || crate::lifecycle::task_is_dead(self.task) {
            // Death may be observed before the status becomes waitable. Let the
            // watcher finish handing the tracee back/reaping before cancelling it.
            if let Some(watcher) = &self.exit_watcher {
                let _ = watcher.wait(KILL_REAP_TIMEOUT);
            }
            self.exit_watcher = None;
            let _ = self.stop_exception_handler(false);
            self.release_ports();
        } else if let Err(err) = self.detach_target() {
            tracing::error!("backend drop could not detach: {err}");
        }
    }
}
