//! Task suspends, port release, and the ptrace attach/detach handshakes.

#[allow(clippy::wildcard_imports)]
use super::*;

impl MacOSBackend
{
    /// Balance every `task_suspend` the backend holds with `task_resume`, ignoring
    /// failures (used on teardown paths).
    pub(super) fn release_suspends(&mut self)
    {
        let count = std::mem::take(&mut lock_shared(&self.exception_state).suspends);
        for _ in 0..count {
            unsafe {
                let _ = task_resume(self.task);
            }
        }
    }

    /// Balance the suspends owned by this resume. If it fails, put back those
    /// already balanced so the caller can restore the backend's stopped state.
    ///
    /// When putting them back fails too, the task may already run: the error is
    /// [`ResumeTaskError::Partial`] with the suspends the backend still holds, and the
    /// caller must treat the target as running (#250 W11).
    pub(super) fn resume_task(&self, count: u32) -> std::result::Result<(), ResumeTaskError>
    {
        // Test hook (#250): one extra suspend, so the last `task_resume` below runs the
        // task before its failure is reported, and the suspends cannot be put back.
        // SAFETY: the backend owns this task right.
        let partway = self.reply_faults.resume_partway.swap(false, Ordering::SeqCst)
            && unsafe { task_suspend(self.task) } == KERN_SUCCESS;
        let total = count + u32::from(partway);
        for done in 0..total {
            let kr = if self.reply_faults.resume.swap(false, Ordering::SeqCst) {
                mach2::kern_return::KERN_FAILURE
            } else {
                // SAFETY: the backend owns this task right and suspend.
                let kr = unsafe { task_resume(self.task) };
                if partway && done + 1 == total && kr == KERN_SUCCESS {
                    return Err(ResumeTaskError::Partial {
                        error: BackendError::Os {
                            code: -1,
                            message: "injected task_resume failed after the task resumed".into(),
                        },
                        held: 0,
                    });
                }
                kr
            };
            if kr != KERN_SUCCESS {
                let error = mach_error(kr, "task_resume failed");
                // The suspends not yet balanced are still held, plus those put back.
                let mut held = total - done;
                let mut restored = true;
                for _ in 0..done {
                    // SAFETY: restore the suspends balanced above, if the task is still alive.
                    if unsafe { task_suspend(self.task) } == KERN_SUCCESS {
                        held += 1;
                    } else {
                        restored = false;
                    }
                }
                return Err(if restored {
                    ResumeTaskError::Restored(error)
                } else {
                    ResumeTaskError::Partial { error, held }
                });
            }
        }
        Ok(())
    }

    /// Deallocate the task and thread ports and forget the process.
    ///
    /// Thread suspends and single-step bits still recorded are balanced first, while the
    /// thread ports are still held (failures, for a dead process, are ignored).
    pub(super) fn release_ports(&mut self)
    {
        use tracing::debug;

        self.undo_thread_actions();

        unsafe {
            debug!("Deallocating {} thread ports", self.threads.len());
            for thread in &self.threads {
                let _ = ffi::mach_port_deallocate(mach_task_self(), *thread);
            }
            if self.task != 0 {
                debug!("Deallocating task port");
                let _ = ffi::mach_port_deallocate(mach_task_self(), self.task);
            }
        }

        self.task = 0;
        self.threads.clear();
        self.pid = ProcessId(0);
        self.architecture = Architecture::current();
        self.launched = false;
        self.child_cleanup = launch::ChildCleanup::default();
        self.traced = false;
        self.state = BackendState::NoProcess;
        self.memory_cache.clear();
        self.mapped_uuids.clear();
        *lock_shared(&self.exception_state) = ExceptionSharedState::new();
    }

    /// Kill and reap a process this backend spawned but could not take control of.
    pub(super) fn discard_child(pid: libc::pid_t) -> BackendResult<()>
    {
        // SAFETY: posix_spawn returned our unreaped child; its pid cannot be reused.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(backend_error(err.into()));
            }
        }
        launch::reap_child(pid, KILL_REAP_TIMEOUT)
    }

    /// Establish signal tracing and wait for its private SIGSTOP exception. Darwin's
    /// PT_ATTACHEXC consumes a pre-existing task suspend (including suspended spawn).
    pub(super) fn start_tracing(&mut self) -> BackendResult<()>
    {
        let (ready, wait) = mpsc::channel();
        lock_shared(&self.exception_state).arm_handshake(ready);
        self.ptrace(ffi::ptrace_requests::PT_ATTACHEXC, 0, 0)?;
        self.traced = true;
        if self.launched {
            self.child_cleanup.traced();
        }
        let arrived = wait.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
            code: -1,
            message: "ptrace attach SIGSTOP did not arrive within 5s".into(),
        })?;
        // Suppress and answer the attach signal while retaining the task suspend.
        // Holding this exception indefinitely would defer an external SIGKILL.
        let ports = lock_shared(&self.exception_state).soft_signals.clone();
        if let Err(err) = arrived {
            // The loop could not suspend the task for the private stop (#191) and holds
            // it unanswered. Suppress and answer it, so the target runs on and the
            // caller's cleanup can stop it with a handshake of its own.
            for (port, _) in ports {
                let _ = self.ptrace(ffi::ptrace_requests::PT_THUPDATE, port, 0);
            }
            if let Some(events) = &self.events_tx {
                exception::answer_held_exceptions(
                    &mut lock_shared(&self.exception_state),
                    &[],
                    &self.reply_faults,
                    events,
                    self.task,
                );
            }
            let _ = self.requeue_handshake_signals(true);
            return Err(err);
        }
        for (port, _) in ports {
            self.ptrace(ffi::ptrace_requests::PT_THUPDATE, port, 0)?;
        }
        let (ack, replied) = mpsc::channel();
        self.exception_commands
            .as_ref()
            .expect("handler started")
            .send(ExceptionLoopCommand::Continue {
                ack,
                keep: Vec::new(),
                discard_deferred: false,
            })
            .map_err(|_| BackendError::Os {
                code: -1,
                message: "attach reply channel closed".into(),
            })?;
        exception::wake_loop(self.exception_port);
        replied.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
            code: -1,
            message: "attach reply timed out".into(),
        })?;
        self.requeue_handshake_signals(true)?;
        self.exit_watcher = Some(crate::lifecycle::ExitWatcher::start(
            self.pid.0 as libc::pid_t,
            Arc::clone(&self.exception_state),
            self.events_tx.as_ref().expect("handler started").clone(),
            self.launched.then(|| self.child_cleanup.clone()),
            self.reply_faults.clone(),
        )?);
        Ok(())
    }

    /// Issue one Darwin tracing request; the address encodes a thread port, never memory.
    pub(super) fn ptrace(&self, request: c_int, thread: thread_act_t, signal: c_int) -> BackendResult<()>
    {
        // SAFETY: this backend owns the pid's tracing relationship (or is attaching);
        // Darwin interprets the address as a Mach port name for PT_THUPDATE only.
        let result = unsafe {
            ffi::ptrace(
                request,
                self.pid.0 as libc::pid_t,
                thread as usize as *mut libc::c_char,
                signal,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        Err(backend_error(DebuggerError::Io(err)))
    }

    /// Select a signal for a logical stop whose kernel exception may already be
    /// answered. Keep queuing and its internal-stop record atomic with respect to
    /// the exception loop, including when XNU delivers the new signal immediately.
    pub(super) fn select_resumed_signal(
        &self,
        shared: &mut ExceptionSharedState,
        port: thread_act_t,
        original: c_int,
        signal: c_int,
    ) -> BackendResult<()>
    {
        if self.reply_faults.signal_selection.swap(false, Ordering::SeqCst) {
            return Err(BackendError::Os {
                code: -1,
                message: "injected signal selection failure".into(),
            });
        }
        let reply_pending = shared.has_signal_reply(port);
        let requeue = signal != 0 && (!reply_pending || signal != original);
        let queued = if requeue {
            Some(exception::RequeuedSignal::new(port, signal)?)
        } else {
            None
        };
        if reply_pending {
            // Private handshakes and failed early suppressions still own a reply. Update
            // it before queuing a new signal, so a failed update never leaves a signal
            // queued in the kernel that no record tracks (#242).
            //
            // On a forwarding resume this runs while other threads run (#250 W10). It
            // cannot race another thread's signal for the per-process `p_xstat`: in XNU
            // `bsd/kern/kern_sig.c` (apple-oss-distributions/xnu f6217f891ac0),
            // `issignal_locked` sets `p->sigwait` before the waiting thread raises its
            // `EXC_SOFT_SIGNAL` and clears it only after the reply, under `proc_lock`,
            // right before it reads `p_xstat`. Every other thread entering
            // `issignal_locked` blocks in `sig_try_locked` while `p->sigwait` is set, so
            // the thread owing this reply is the only one that can stop for the
            // debugger; `psignal_internal` sends a traced process's signals to that
            // path without touching `p_xstat`, and `PT_THUPDATE` (`mach_process.c`)
            // writes it under `proc_lock`.
            self.ptrace(ffi::ptrace_requests::PT_THUPDATE, port, if requeue { 0 } else { signal })?;
        }
        if let Some(queued) = queued {
            // The original was suppressed before its early reply. Queue the chosen
            // signal on the same thread so XNU evaluates its properties afresh.
            // Track SIGKILL too: thread-directed delivery can raise a soft exception.
            // SAFETY: the stop record retains a thread right belonging to our tracee.
            if unsafe { ffi::__pthread_kill(port, signal) } != 0 {
                return Err(backend_error(DebuggerError::Io(std::io::Error::last_os_error())));
            }
            shared.requeued_signals.push(queued);
        }
        Ok(())
    }

    /// End the trace: PT_DETACH requires the tracee to wait in a signal exception, not
    /// just a task_suspend, so the backend raises a private SIGSTOP and detaches while its
    /// exception is held. Answering that exception afterwards stops the process for real;
    /// [`detach_target`](Self::detach_target) continues it once the reply is sent.
    pub(super) fn stop_tracing(&mut self, private_stop_sent: &mut bool) -> BackendResult<()>
    {
        let fail_handshake = self
            .reply_faults
            .detach_handshake
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        if !self.traced {
            return Ok(());
        }
        // Discard client decisions already held when detach began. New signals
        // arriving during the handshake are preserved independently by the loop.
        let (ready, wait) = mpsc::channel();
        let pending = {
            let mut shared = lock_shared(&self.exception_state);
            if shared.dead {
                return Err(BackendError::NotFound(format!("process {} has exited", self.pid.0)));
            }
            shared.arm_handshake(ready);
            self.reply_faults.detach_handshake_armed();
            shared.pending_exception
        };
        if pending {
            let (ack, replied) = mpsc::channel();
            self.exception_commands
                .as_ref()
                .expect("handler started")
                .send(ExceptionLoopCommand::Continue {
                    ack,
                    keep: Vec::new(),
                    discard_deferred: true,
                })
                .map_err(|_| BackendError::Os {
                    code: -1,
                    message: "detach reply channel closed".into(),
                })?;
            exception::wake_loop(self.exception_port);
            replied.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
                code: -1,
                message: "detach reply timed out".into(),
            })?;
        } else if let Some(events) = &self.events_tx {
            exception::answer_held_exceptions(
                &mut lock_shared(&self.exception_state),
                &[],
                &self.reply_faults,
                events,
                self.task,
            );
        }
        while self.try_next_event().is_some() {}
        // Test hook (#198): send the private SIGSTOP only after a paused suspend retry of
        // the loop has run, so its exception cannot arrive before that retry's suspend.
        let defer_stop = self.reply_faults.defer_detach_stop();
        if !defer_stop {
            self.send_private_stop(private_stop_sent)?;
        }
        // Backend state can still be Running while the exception loop already owns
        // a pending stop. Release physical holds regardless of the last delivered event.
        self.release_suspends();
        self.reply_faults.release_suspend_retry_for_detach();
        if defer_stop {
            self.reply_faults.release_suspend_retry_before_detach_stop();
            self.send_private_stop(private_stop_sent)?;
        }
        // A failed task suspend still holds the private SIGSTOP (#191): the fallback
        // PT_DETACH of `detach_target` finds the tracee waiting in its exception.
        wait.recv_timeout(EXCEPTION_REPLY_TIMEOUT).map_err(|_| BackendError::Os {
            code: -1,
            message: "detach SIGSTOP did not arrive within 5s".into(),
        })??;
        if fail_handshake {
            return Err(BackendError::Os {
                code: -1,
                message: "injected detach handshake failure".into(),
            });
        }
        // PT_DETACH needs the tracee waiting in its signal exception (EBUSY otherwise),
        // so it comes before the reply.
        self.detach_trace()
    }

    /// Raise detach's private SIGSTOP, unless the process has exited.
    pub(super) fn send_private_stop(&self, private_stop_sent: &mut bool) -> BackendResult<()>
    {
        let shared = lock_shared(&self.exception_state);
        if shared.dead {
            return Err(BackendError::NotFound(format!("process {} has exited", self.pid.0)));
        }
        self.reply_faults.sample_detach_suspend_count();
        self.reply_faults.report_detach_stop_awaited(shared.internal_stop.is_some());
        // SAFETY: the shared lock excludes the watcher's reaping of this pid.
        if unsafe { libc::kill(self.pid.0 as libc::pid_t, libc::SIGSTOP) } != 0 {
            return Err(backend_error(DebuggerError::Io(std::io::Error::last_os_error())));
        }
        *private_stop_sent = true;
        Ok(())
    }

    /// End tracing and account for the kernel resume, including fallback cleanup.
    pub(super) fn detach_trace(&mut self) -> BackendResult<()>
    {
        self.ptrace(ffi::ptrace_requests::PT_DETACH, 0, 0)?;
        // XNU's PT_DETACH clears P_LSIGEXC and calls task_resume while the
        // private signal is waiting. It consumes one of our task suspensions.
        // Account for that before teardown balances the rest: a second resume
        // could otherwise consume the job-control hold as it is being placed.
        {
            let mut shared = lock_shared(&self.exception_state);
            shared.suspends = shared.suspends.saturating_sub(1);
        }
        self.traced = false;
        Ok(())
    }

    /// Deliver signals that arrived during the private handshake on their original
    /// threads. Attach still owns a task suspend; detach still holds its private reply.
    /// Signal numbers currently held from the detach handshake, in replay order.
    fn held_handshake_signal_numbers(&self) -> Vec<i32>
    {
        lock_shared(&self.exception_state)
            .handshake_signals
            .as_ref()
            .map(|signals| signals.iter().map(|signal| signal.signal).collect())
            .unwrap_or_default()
    }

    pub(super) fn requeue_handshake_signals(&self, traced: bool) -> BackendResult<()>
    {
        let mut shared = lock_shared(&self.exception_state);
        let mut failure = None;
        for signal in shared.handshake_signals.take().unwrap_or_default() {
            let error = if self
                .reply_faults
                .handshake_requeue
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Some(std::io::Error::from_raw_os_error(libc::ESRCH))
            } else {
                // SAFETY: the saved signal owns an independent tracee thread send right.
                (unsafe { ffi::__pthread_kill(signal.thread, signal.signal) } != 0).then(std::io::Error::last_os_error)
            };
            if let Some(error) = error {
                failure.get_or_insert_with(|| backend_error(DebuggerError::Io(error)));
                continue;
            }
            if traced {
                shared.requeued_signals.push(signal);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Shut down tracing, restore exception handlers and balance our suspensions.
    ///
    /// The teardown always runs, even when ending the trace fails (for example the
    /// private SIGSTOP never arrives because another stop won the race): skipping it
    /// would leak the handler thread, its port and the task rights and leave the target
    /// suspended. In that case `PT_DETACH` is still attempted. Failures from tracing
    /// and handler restoration are returned together as `DetachCleanup` afterwards.
    pub(super) fn detach_target(&mut self) -> BackendResult<()>
    {
        // Before the private SIGSTOP: the kernel could otherwise pick a thread that is
        // still `thread_suspend`ed to take it, and the detach would time out. Restore
        // birth-trap instructions first, so a newborn resumes on its original `stp`
        // rather than a `brk` this backend is about to stop handling. No thread of a
        // released process may be left single-stepping either.
        let birth = self.disarm_birth();
        self.undo_thread_actions();
        let mut private_stop_sent = false;
        let traced = self.stop_tracing(&mut private_stop_sent);
        let fallback = if traced.is_err() && self.traced {
            self.detach_trace()
        } else {
            Ok(())
        };
        // Replay and continuation belong to every successful PT_DETACH, including
        // fallback after a handshake error. Preserve that error in the return value.
        let replayed = if !self.traced {
            let held = self.held_handshake_signal_numbers();
            if private_stop_sent {
                // Below, detach continues the released process with SIGCONT, which undoes
                // any job-control stop. A SIGSTOP cannot be caught, and SIGTSTP, SIGTTIN
                // and SIGTTOU at their default action stop the process just the same, so
                // replaying one changes nothing but lets its stop take effect before the
                // private one is even answered. Detach's wait would take that stop for its
                // own and send its SIGCONT before the private stop lands, leaving the
                // process stopped (#311, #319). Drop those instead; a stop signal the
                // target handles is replayed so its handler runs.
                //
                // Still open: a stop another process sends between PT_DETACH and the
                // private reply is not held here, and can win the same race.
                let pid = self.pid.0 as libc::pid_t;
                if let Some(signals) = lock_shared(&self.exception_state).handshake_signals.as_mut() {
                    signals.retain(|signal| !crate::lifecycle::stops_without_handler(pid, signal.signal));
                }
            }
            let replayed = self.held_handshake_signal_numbers();
            self.reply_faults
                .report_detach_replay(|| crate::exception::DetachReplay { held, replayed });
            self.requeue_handshake_signals(false)
        } else {
            Ok(())
        };
        self.exit_watcher = None;
        self.child_cleanup.detach(self.traced);
        let stop_fault = self.reply_faults.take_detach_stop_wait();
        let hide_stop = stop_fault.as_ref().is_some_and(|fault| fault.before_continue.is_some());
        // Test hook: send the SIGCONT of a timed-out wait before the private SIGSTOP is
        // even answered, so the stop is certain to land after it.
        let expire_wait = stop_fault.is_some() && !hide_stop && private_stop_sent && !self.traced;
        if expire_wait {
            self.send_detach_sigcont();
        }
        let restored = self.stop_exception_handler(true);
        self.release_suspends();
        let mut stop_unconfirmed = Ok(());
        if private_stop_sent && !self.traced {
            // The handler answered the private SIGSTOP exception after PT_DETACH, so the
            // kernel takes the SIGSTOP as an ordinary job-control stop (state `T`) once the
            // thread processes the reply. Wait (bounded) until it has, so the SIGCONT
            // cannot overtake the stop. SSTOP alone precedes the kernel task hold;
            // wait for that hold too, then undo our own stop: the released process must
            // keep running. A program with a SIGCONT handler sees one extra SIGCONT.
            //
            // If the wait runs out first (the target not scheduled for over a second),
            // the SIGCONT can overtake the stop, which then leaves the process stopped
            // (#149). Keep watching for a bounded time and continue it again once the
            // stop lands; if it is never seen, report that it may stay stopped.
            let pid = self.pid.0 as libc::pid_t;
            let real_stop = || crate::lifecycle::job_control_stop_has_taken_effect(pid, self.task);
            let landed = || !hide_stop && real_stop();
            let stop_timeout = if stop_fault.is_some() {
                Duration::ZERO
            } else {
                DETACH_STOP_TIMEOUT
            };
            let stop_recheck = stop_fault.as_ref().map_or(DETACH_STOP_RECHECK, |fault| fault.recheck);
            let mut observed = false;
            if !expire_wait {
                observed = wait_until(stop_timeout, landed);
                if let Some((timeout, probe)) = stop_fault.and_then(|fault| fault.before_continue) {
                    let _ = probe.send(wait_until(timeout, real_stop));
                }
                self.send_detach_sigcont();
            }
            if !observed {
                observed = wait_until(stop_recheck, landed);
                if observed {
                    self.send_detach_sigcont();
                }
            }
            // SAFETY: signal 0 only checks that the pid this backend holds still exists.
            if !observed && unsafe { libc::kill(pid, 0) } == 0 {
                stop_unconfirmed = Err(BackendError::Os {
                    code: -1,
                    message: format!(
                        "detach did not see its private SIGSTOP take effect within {:?}; process {} may stay stopped",
                        stop_timeout + stop_recheck,
                        self.pid.0
                    ),
                });
            }
        }
        self.release_ports();
        let failures: Vec<_> = [birth, traced, replayed, fallback, restored, stop_unconfirmed]
            .into_iter()
            .filter_map(|result| result.err())
            .map(|err| err.to_string())
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(BackendError::DetachCleanup(failures.join("; ")))
        }
    }

    /// Continue the released process after detach's private SIGSTOP.
    pub(super) fn send_detach_sigcont(&self)
    {
        // SAFETY: plain kill(2) on the pid this backend still holds (not yet released).
        if unsafe { libc::kill(self.pid.0 as libc::pid_t, libc::SIGCONT) } != 0 {
            tracing::warn!(
                "SIGCONT after detach failed: {}; process {} may stay stopped",
                std::io::Error::last_os_error(),
                self.pid.0
            );
        }
    }
}
