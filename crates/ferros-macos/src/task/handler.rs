//! Taking the task port and installing or removing the exception handler.

#[allow(clippy::wildcard_imports)]
use super::*;

impl MacOSBackend
{
    /// Obtain the Mach task port and thread list for `pid` and start the exception
    /// handler.
    ///
    /// Does not change the task's execution state; callers decide whether to suspend.
    /// On error nothing is kept.
    ///
    /// ## Mach APIs Used
    ///
    /// - **task_for_pid()**: Obtains a Mach task port for the process
    /// - **task_threads()**: Enumerates all threads in the task
    ///
    /// ## Permissions
    ///
    /// Requires root or the debugger entitlement to call `task_for_pid()`.
    ///
    /// See:
    /// - [task_for_pid(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_for_pid/)
    /// - [task_threads(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_threads/)
    pub(super) fn attach_task(&mut self, pid: ProcessId) -> Result<()>
    {
        use tracing::{debug, info, trace};

        info!("Attaching to process {}", pid.0);
        debug!("Getting Mach task port for process {}", pid.0);

        unsafe {
            let mut task: mach_port_t = 0;
            trace!("Calling task_for_pid for PID {}", pid.0);
            let result = if std::mem::take(&mut self.fail_task_for_pid) {
                libc::KERN_FAILURE
            } else {
                ffi::task_for_pid(mach_task_self(), pid.0 as c_int, &mut task)
            };

            if result != KERN_SUCCESS {
                if result == libc::KERN_FAILURE {
                    let process_exists = libc::kill(pid.0 as libc::pid_t, 0) == 0;
                    // Only ESRCH proves the pid is gone: EPERM means the process exists but
                    // belongs to another user. Read errno before anything else can change it.
                    let no_such_process =
                        !process_exists && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);

                    if process_exists {
                        return Err(DebuggerError::PermissionDenied(format!(
                            "task_for_pid() failed with KERN_FAILURE, but process {} exists.\n\nFrom the repository root, \
                             sign the debugger with the `com.apple.security.cs.debugger` entitlement using `just \
                             sign-debugger <path-to-ferros>`, then retry with the signed binary.\n\nTo start a new process \
                             under the debugger, use `ferros launch <program>`.",
                            pid.0
                        )));
                    }

                    if no_such_process {
                        return Err(DebuggerError::ProcessNotFound(pid.0));
                    }
                }

                return Err(mach_error(result, "task_for_pid").into());
            }

            let mut thread_list: *mut thread_act_t = std::ptr::null_mut();
            let mut thread_count: mach_msg_type_number_t = 0;

            let result = task_threads(task, &mut thread_list, &mut thread_count);
            if result != KERN_SUCCESS || thread_count == 0 {
                threads::ThreadManager::deallocate_threads_array(thread_list, thread_count);
                let _ = ffi::mach_port_deallocate(mach_task_self(), task);
                return Err(if result == KERN_SUCCESS {
                    BackendError::NotFound(format!("process {} has no threads", pid.0)).into()
                } else {
                    mach_error(result, "task_threads").into()
                });
            }

            let slice = std::slice::from_raw_parts(thread_list, thread_count as usize);
            self.task = task;
            self.pid = pid;
            self.threads = slice.to_vec();
            threads::ThreadManager::deallocate_threads_array(thread_list, thread_count);
        }

        *lock_shared(&self.exception_state) = ExceptionSharedState::new();
        self.memory_cache.clear();
        self.mapped_uuids.clear();
        self.peeked = None;

        if let Err(err) = self.start_exception_handler() {
            let _ = self.stop_exception_handler(true);
            self.release_ports();
            return Err(err);
        }

        Ok(())
    }

    /// Start the Mach exception handler thread.
    ///
    /// This creates a Mach receive port, registers it with `task_swap_exception_ports()`
    /// (saving the ports it replaces for [`stop_exception_handler`](Self::stop_exception_handler)
    /// to restore), creates the stop event channel and spawns a background thread to handle
    /// exceptions (breakpoints, faults, etc.).
    ///
    /// ## Mach APIs Used
    ///
    /// - **mach_port_allocate()**: Creates a Mach receive port
    /// - **mach_port_insert_right()**: Makes the port sendable
    /// - **task_swap_exception_ports()**: Registers the exception port with the task and
    ///   returns the previous ones
    ///
    /// ## Exception Mask
    ///
    /// The handler receives:
    /// - `EXC_MASK_BREAKPOINT`: Software breakpoints (INT3/BRK)
    /// - `EXC_MASK_BAD_ACCESS`: Memory access violations
    /// - `EXC_MASK_BAD_INSTRUCTION`: Illegal instructions
    /// - `EXC_MASK_ARITHMETIC`: Arithmetic exceptions (div by zero, overflow)
    /// - `EXC_MASK_SOFTWARE`: Software-generated exceptions
    ///
    /// ## Errors
    ///
    /// Returns `DebuggerError::Backend` keeping the `kern_return_t` if port allocation or
    /// exception port registration fails.
    /// Returns `Io` with the OS error if the handler thread cannot be spawned.
    ///
    /// ## See Also
    ///
    /// - `stop_exception_handler()`: Stops the exception handler
    /// - `exception::run_exception_loop()`: The exception handling loop
    /// - [task_set_exception_ports(3) man page](https://developer.apple.com/documentation/kernel/1402149-task_set_exception_ports/)
    pub(super) fn start_exception_handler(&mut self) -> Result<()>
    {
        use tracing::info;

        let _ = self.stop_exception_handler(true);

        if self.task == MACH_PORT_NULL {
            return Ok(());
        }

        unsafe {
            let self_task = mach_task_self();
            let mut port: mach_port_t = MACH_PORT_NULL;
            let mut kr = mach_port_allocate(self_task, MACH_PORT_RIGHT_RECEIVE, &mut port);
            if kr != KERN_SUCCESS {
                return Err(mach_error(kr, "mach_port_allocate").into());
            }

            kr = mach_port_insert_right(self_task, port, port, MACH_MSG_TYPE_MAKE_SEND);
            if kr != KERN_SUCCESS {
                let _ = mach_port_destroy(self_task, port);
                return Err(mach_error(kr, "mach_port_insert_right").into());
            }

            let mask: exception_mask_t = EXC_MASK_BREAKPOINT
                | EXC_MASK_BAD_ACCESS
                | EXC_MASK_BAD_INSTRUCTION
                | EXC_MASK_ARITHMETIC
                | EXC_MASK_SOFTWARE;
            let behavior: exception_behavior_t = (EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES) as exception_behavior_t;
            let flavor = exception::thread_state_flavor_for_arch(self.architecture);
            let saved = match SavedExceptionPorts::install(self.task, mask, port, behavior, flavor) {
                Ok(saved) => saved,
                Err(kr) => {
                    let _ = mach_port_destroy(self_task, port);
                    return Err(mach_error(kr, "task_swap_exception_ports").into());
                }
            };
            // From here on, `stop_exception_handler` restores the saved ports and destroys
            // the port, including when spawning the thread fails.
            self.saved_exception_ports = Some(saved);
            self.exception_port = port;

            let (command_tx, command_rx) = mpsc::channel();
            let (events_tx, events_rx) = mpsc::channel();
            let loop_events = events_tx.clone();
            let shared_state = Arc::clone(&self.exception_state);
            let architecture = self.architecture;
            let task = self.task;
            let pid = self.pid.0 as libc::pid_t;
            let faults = self.reply_faults.clone();
            info!("Spawning Mach exception handler thread");
            let handle = thread::Builder::new()
                .name("ferros-mac-exc".to_string())
                .spawn(move || {
                    exception::run_exception_loop(
                        port,
                        task,
                        pid,
                        command_rx,
                        shared_state,
                        architecture,
                        loop_events,
                        faults,
                    );
                })
                .map_err(DebuggerError::Io)?;

            self.exception_thread = Some(handle);
            self.exception_commands = Some(command_tx);
            self.events_tx = Some(events_tx);
            self.events_rx = Some(events_rx);
        }

        Ok(())
    }

    /// Stop the exception handler thread.
    ///
    /// With `restore`, the target's saved exception ports are put back first, so new
    /// exceptions go to their old handlers; without it (the process is being killed) the
    /// saved rights are only released. Held exceptions are then replied to, so their
    /// threads can continue once the task is resumed. Queued stop events are discarded.
    pub(super) fn stop_exception_handler(&mut self, restore: bool) -> BackendResult<()>
    {
        // Test hook (#198): a real exception queued on the port just before teardown.
        let parked =
            self.exception_thread.is_some() && self.reply_faults.queue_teardown_exception(self.task, self.exception_port);
        let mut restored = Ok(());
        if let Some(saved) = self.saved_exception_ports.take() {
            if restore && self.task != MACH_PORT_NULL {
                restored = saved.restore(self.task, &self.reply_faults);
            }
            drop(saved);
        }

        // If the loop holds exceptions (a pending stop, kept records, or a batch held
        // behind a failed task_suspend, #191) let it reply, answer the exceptions still
        // queued on the port and exit before the port is destroyed: a message destroyed
        // with the port is an unhandled exception. Otherwise it is blocked receiving,
        // and destroying the port is what wakes it.
        let holds_exception = {
            let shared = lock_shared(&self.exception_state);
            shared.pending_exception || !shared.held.is_empty() || shared.held_unsuspended || shared.interrupt_suspend
        };
        if let Some(tx) = self.exception_commands.take()
            && tx.send(ExceptionLoopCommand::Shutdown).is_ok()
        {
            exception::wake_loop(self.exception_port);
        }
        if parked {
            self.reply_faults.unpark_loop();
        }
        if holds_exception && let Some(handle) = self.exception_thread.take() {
            let _ = handle.join();
        }

        if self.exception_port != MACH_PORT_NULL {
            unsafe {
                let _ = mach_port_destroy(mach_task_self(), self.exception_port);
            }
            self.exception_port = MACH_PORT_NULL;
        }

        if let Some(handle) = self.exception_thread.take() {
            let _ = handle.join();
        }

        self.events_tx = None;
        self.events_rx = None;
        self.peeked = None;
        // The loop answers what it holds before it exits; this only catches exceptions
        // left behind by a loop that never ran.
        exception::release_held_exceptions(&mut lock_shared(&self.exception_state));
        if let Err(err) = &restored {
            tracing::warn!("{err}");
        }
        restored
    }
}
