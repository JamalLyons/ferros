//! # Session: the portable debugging engine
//!
//! A [`Session`] owns one [`Backend`] (as a `Box<dyn Backend>`) and adds the *policy*
//! that is the same on every operating system. The backend only provides *mechanism*.
//!
//! | Concern                                         | Policy here (`Session`)                              | Mechanism (`Backend`)                    |
//! |-------------------------------------------------|------------------------------------------------------|------------------------------------------|
//! | Which calls are legal                           | Typed errors before the backend is called            | `BackendError::InvalidState` as a guard  |
//! | Threads                                         | Selected thread, follows the stopping thread         | `threads`                                |
//! | Software breakpoints                            | Breakpoint table, original bytes, hit counts         | `read_memory`, `write_memory`            |
//! | Breakpoint stops                                | Map the pc with `Arch::pc_after_trap`, count hits    | Report the raw pc                        |
//! | Stack traces and symbols                        | Symbol cache, unwinder                               | `read_registers`, `read_memory`, `images` |
//! | Process control                                 | State tracking, drain pending events before resuming | `launch`, `attach`, `detach`, `resume`, `wait` |
//!
//! ## State machine
//!
//! ```text
//!  NotStarted --launch/attach--> Stopped --resume/resume_with_signal--> Running
//!      ^                          ^  |                                    |
//!      |                          |  |   wait returns a non-terminal event (e.g. after interrupt)
//!      |                          +--|------------------------------------+
//!      +------- detach --------------+                                    |
//!  Exited <------- wait returns a terminal event -------------------------+
//!  Exited <------- kill (from Stopped or Running) ------------------------+
//!  Exited --launch/attach--> Stopped
//! ```
//!
//! | Call                                                       | NotStarted / Exited | Stopped             | Running        |
//! |------------------------------------------------------------|---------------------|---------------------|----------------|
//! | `launch`, `attach`                                         | yes                 | `AlreadyAttached`   | `AlreadyAttached` |
//! | `detach`, `resume`, `resume_with_signal`                   | `NotAttached`       | yes                 | `NotStopped`   |
//! | `interrupt`                                                | `NotAttached`       | yes (no-op)         | yes            |
//! | `kill`                                                     | `NotAttached`       | yes                 | yes            |
//! | `threads`, `select_thread`, registers, memory, regions     | `NotAttached`       | yes                 | `NotStopped`   |
//! | breakpoint add/remove/enable/disable                       | `NotAttached`       | yes                 | `NotStopped`   |
//! | `stack_trace`, `symbolicate`, `images`                     | `NotAttached`       | yes                 | `NotStopped`   |
//! | `breakpoint_info`                                          | `NotAttached`       | yes                 | yes            |
//! | `wait(Some(timeout))`                                      | `NotAttached`       | yes (drains events) | yes            |
//! | `wait(None)`                                               | `NotAttached`       | pending event, else `NotRunning` | yes |
//! | `state`, `pid`, `architecture`, `last_stop`, `breakpoints` | yes                 | yes                 | yes            |
//!
//! Each error in the table is a [`DebuggerError`] variant and is returned before the
//! backend is called. A call that fails leaves the session in the state it was in.
//!
//! - After `launch` or `attach` the first thread is selected and [`Session::last_stop`]
//!   is `None` (the initial stop has no event).
//! - On a stop event that names a thread, that thread is selected. If the selected
//!   thread no longer exists after a stop, the first thread is selected.
//! - A terminal event ([`StopEvent::is_terminal`]) moves the session to
//!   [`SessionState::Exited`]: the pid, thread list and selection are cleared and the
//!   breakpoint table is emptied (the process memory that held the traps is gone).
//!
//! - [`Session::kill`] also moves the session to [`SessionState::Exited`], with
//!   [`Session::last_stop`] set to `StopEvent::Killed { signal: SIGKILL }` (the backend
//!   reports no event for a kill).
//!
//! ## Ownership
//!
//! A process the session launched belongs to it: dropping the session kills it. A
//! process the session attached to does not: dropping the session restores every trap
//! and detaches, leaving it running (a running process is interrupted first, waiting at
//! most [`DROP_STOP_TIMEOUT`]). Drop ignores failures; call [`Session::kill`] or
//! [`Session::detach`] to end a session deliberately and see errors.
//! [`Session::ownership`] tells which rule applies.
//!
//! ## Known gaps
//!
//! These are deliberate for now and are fixed by later tasks:
//!
//! - T07 (issue #33): [`Session::resume`] from a breakpoint stop just continues. It does
//!   not step over the trap, so the thread hits the same trap again.
//! - T07 (issue #33): [`Session::read_memory`] returns memory as it is, including the
//!   trap bytes of inserted breakpoints.
//! - T07 (issue #33): [`Session::write_memory`] over an inserted trap bypasses the
//!   breakpoint table (the saved original bytes are not updated).
//! - T07 (issue #33): `Session` has no `step_instruction` yet.

#![forbid(unsafe_code)]

mod breakpoints;
mod images;
mod lifecycle;
mod stack;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::backend::{Backend, StopEvent};
use crate::breakpoints::BreakpointStore;
use crate::error::{DebuggerError, Result};
use crate::symbols::SymbolCache;
use crate::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, ThreadId};

/// Where a [`Session`] is in its state machine. See the [module documentation](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState
{
    /// No process yet, or the last one was detached.
    NotStarted,
    /// The process exists and every thread is stopped.
    Stopped,
    /// The process was resumed and has not reported a stop yet.
    Running,
    /// The process exited or was killed; a new `launch` or `attach` is allowed.
    Exited,
}

/// Portable debugging engine driving one [`Backend`].
///
/// See the [module documentation](self) for the state machine and the division of
/// labour with the backend. A `Session` is [`Send`]: it can be moved to the thread that
/// issues every call, as the backend's threading rules require.
pub struct Session
{
    backend: Box<dyn Backend>,
    state: SessionState,
    ownership: Option<ProcessOwnership>,
    pid: Option<ProcessId>,
    threads: Vec<ThreadId>,
    selected: Option<ThreadId>,
    last_stop: Option<StopEvent>,
    /// A stop event drained from the backend by `resume`, returned by the next `wait`.
    deferred: Option<StopEvent>,
    breakpoints: BreakpointStore,
    symbols: SymbolCache,
    /// Images (path, load address) already given to the symbol cache, loaded or not.
    loaded_images: HashSet<(PathBuf, u64)>,
}

/// How the session got its process, which decides what dropping the session does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOwnership
{
    /// Started by [`Session::launch`]: killed when the session is dropped.
    Launched,
    /// Taken over by [`Session::attach`]: released, still running, when the session is
    /// dropped.
    Attached,
}

/// Signal number of `SIGKILL` (9 on every POSIX system), reported in
/// [`StopEvent::Killed`] by [`Session::kill`].
pub const SIGKILL: i32 = 9;

impl std::fmt::Debug for Session
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        f.debug_struct("Session")
            .field("state", &self.state)
            .field("pid", &self.pid)
            .field("threads", &self.threads)
            .field("selected", &self.selected)
            .field("last_stop", &self.last_stop)
            .field("breakpoints", &self.breakpoints)
            .finish_non_exhaustive()
    }
}

impl Drop for Session
{
    /// Apply the ownership rules: kill a launched process, release an attached one.
    ///
    /// - Launched: the original bytes of every inserted trap are restored while it is
    ///   stopped (harmless, and it keeps the target's memory honest for a backend that
    ///   outlives the call), then the process is killed with [`Backend::kill`], whether
    ///   it is stopped or running.
    /// - Attached: the process is left running without traps. A running process is
    ///   interrupted first and the stop awaited for at most [`DROP_STOP_TIMEOUT`]; once
    ///   stopped, every trap is restored and [`Backend::detach`] releases it. If it
    ///   exits meanwhile there is nothing left to do; if it never stops, `detach` is
    ///   still attempted.
    ///
    /// Failures are ignored and the drop never blocks for longer than the timeout. Call
    /// [`Session::kill`] or [`Session::detach`] to end a session deliberately and see
    /// errors.
    fn drop(&mut self)
    {
        match (self.ownership, self.state) {
            (Some(ProcessOwnership::Launched), SessionState::Stopped) => {
                let _ = self.restore_all_traps();
                let _ = self.backend.kill();
            }
            (Some(ProcessOwnership::Launched), SessionState::Running) => {
                let _ = self.backend.kill();
            }
            (Some(ProcessOwnership::Attached), SessionState::Running) => {
                if self.stop_for_release() {
                    let _ = self.restore_all_traps();
                    let _ = self.backend.detach();
                }
            }
            (Some(ProcessOwnership::Attached), SessionState::Stopped) => {
                let _ = self.restore_all_traps();
                let _ = self.backend.detach();
            }
            _ => {}
        }
    }
}

/// How long dropping a session that is running an attached process waits for the
/// interrupt to stop it before detaching anyway.
pub const DROP_STOP_TIMEOUT: Duration = Duration::from_secs(1);

impl Session
{
    /// Bring a running attached process to a stop so it can be released (used by drop).
    ///
    /// Returns `false` when the process is gone (a terminal event arrived), `true`
    /// otherwise, including when no stop arrived within [`DROP_STOP_TIMEOUT`] (detach is
    /// then still worth trying). Every pending event is drained, so the backend has
    /// nothing queued when it is detached.
    fn stop_for_release(&mut self) -> bool
    {
        // An event drained by `resume` means the backend never resumed: it is stopped.
        if let Some(event) = self.deferred.take() {
            if event.is_terminal() {
                return false;
            }
        } else {
            let _ = self.backend.interrupt();
            let deadline = Instant::now() + DROP_STOP_TIMEOUT;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.backend.wait(Some(remaining)) {
                    Ok(Some(event)) if event.is_terminal() => return false,
                    Ok(Some(_)) => break,
                    Ok(None) | Err(_) if remaining.is_zero() => return true,
                    Ok(None) | Err(_) => {}
                }
            }
        }
        self.state = SessionState::Stopped;
        loop {
            match self.backend.wait(Some(Duration::ZERO)) {
                Ok(Some(event)) if event.is_terminal() => return false,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return true,
            }
        }
    }
    /// A session with no process, driving `backend`.
    ///
    /// The backend should have no process either; the session starts in
    /// [`SessionState::NotStarted`].
    #[must_use]
    pub fn new(backend: Box<dyn Backend>) -> Self
    {
        Self {
            backend,
            state: SessionState::NotStarted,
            ownership: None,
            pid: None,
            threads: Vec::new(),
            selected: None,
            last_stop: None,
            deferred: None,
            breakpoints: BreakpointStore::new(),
            symbols: SymbolCache::new(),
            loaded_images: HashSet::new(),
        }
    }

    /// Current state of the session.
    #[must_use]
    pub fn state(&self) -> SessionState
    {
        self.state
    }

    /// Whether the current process was launched or attached to, or `None` when there is
    /// no process. See [Ownership](self#ownership).
    #[must_use]
    pub fn ownership(&self) -> Option<ProcessOwnership>
    {
        self.ownership
    }

    /// Pid of the debugged process, or `None` when there is no process.
    #[must_use]
    pub fn pid(&self) -> Option<ProcessId>
    {
        self.pid
    }

    /// Architecture of the target, or of the host before a target exists, as reported
    /// by [`Backend::architecture`].
    #[must_use]
    pub fn architecture(&self) -> Architecture
    {
        self.backend.architecture()
    }

    /// The event of the last stop.
    ///
    /// `None` after `launch` and `attach` (the initial stop has no event) and while the
    /// process is running. After a terminal event it holds that event. For a
    /// [`StopEvent::Breakpoint`] the pc is the trap address (already mapped with
    /// [`Arch::pc_after_trap`](crate::arch::Arch::pc_after_trap)).
    #[must_use]
    pub fn last_stop(&self) -> Option<&StopEvent>
    {
        self.last_stop.as_ref()
    }

    /// The backend this session drives.
    #[must_use]
    pub fn backend(&self) -> &dyn Backend
    {
        self.backend.as_ref()
    }

    /// The backend this session drives, mutably.
    ///
    /// This is an escape hatch for tests and diagnostics. Calls made through it bypass
    /// the session: the session state is not updated, and writing or removing trap
    /// bytes through it bypasses the breakpoint table, which then no longer matches
    /// the target's memory.
    pub fn backend_mut(&mut self) -> &mut dyn Backend
    {
        self.backend.as_mut()
    }

    /// Typed error for calls that need a stopped process, decided before any backend call.
    fn require_stopped(&self) -> Result<()>
    {
        match self.state {
            SessionState::Stopped => Ok(()),
            SessionState::Running => Err(DebuggerError::NotStopped),
            SessionState::NotStarted | SessionState::Exited => Err(DebuggerError::NotAttached),
        }
    }

    /// Typed error for calls that need any process, decided before any backend call.
    fn require_process(&self) -> Result<()>
    {
        match self.state {
            SessionState::Stopped | SessionState::Running => Ok(()),
            SessionState::NotStarted | SessionState::Exited => Err(DebuggerError::NotAttached),
        }
    }

    /// Refresh the thread list from the backend and fix the selection.
    ///
    /// `preferred` (the thread a stop event names) is selected if it exists. Otherwise
    /// the current selection is kept if it still exists, else the first thread is
    /// selected. If the backend cannot list threads, the old list cannot be trusted (a
    /// thread in it may have exited): only `preferred` is kept, so a dead thread is never
    /// left selected. The next successful [`threads`](Self::threads) call repairs the list.
    fn sync_threads(&mut self, preferred: Option<ThreadId>)
    {
        match self.backend.threads() {
            Ok(threads) => self.threads = threads,
            Err(error) => {
                tracing::debug!("could not list threads after a stop: {error}");
                self.threads = preferred.into_iter().collect();
            }
        }
        self.fix_selection(preferred);
    }

    fn fix_selection(&mut self, preferred: Option<ThreadId>)
    {
        let exists = |thread: &ThreadId| self.threads.contains(thread);
        self.selected = preferred
            .filter(exists)
            .or_else(|| self.selected.filter(exists))
            .or_else(|| self.threads.first().copied());
    }

    /// Forget everything about the current process (after detach or a terminal event).
    fn clear_process(&mut self)
    {
        self.pid = None;
        self.ownership = None;
        self.threads.clear();
        self.selected = None;
        self.deferred = None;
        self.breakpoints.drain();
    }

    fn selected_or_error(&self) -> Result<ThreadId>
    {
        self.selected.ok_or(DebuggerError::NoThreadSelected)
    }

    /// List the process's threads and refresh the session's thread list.
    ///
    /// If the selected thread no longer exists, the first thread is selected.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error.
    pub fn threads(&mut self) -> Result<Vec<ThreadId>>
    {
        self.require_stopped()?;
        self.threads = self.backend.threads()?;
        self.fix_selection(None);
        Ok(self.threads.clone())
    }

    /// The selected thread, used by [`read_registers`](Self::read_registers),
    /// [`write_registers`](Self::write_registers) and [`stack_trace`](Self::stack_trace).
    #[must_use]
    pub fn selected_thread(&self) -> Option<ThreadId>
    {
        self.selected
    }

    /// Select `thread`, which must be one of the threads known since the last stop
    /// (see [`threads`](Self::threads)).
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::ThreadNotFound`] if `thread` is not one of the
    /// process's threads.
    pub fn select_thread(&mut self, thread: ThreadId) -> Result<()>
    {
        self.require_stopped()?;
        if !self.threads.contains(&thread) {
            return Err(DebuggerError::ThreadNotFound(thread.0));
        }
        self.selected = Some(thread);
        Ok(())
    }

    /// Read the selected thread's registers.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::NoThreadSelected`] if the process has no
    /// thread, or the backend's error.
    pub fn read_registers(&mut self) -> Result<Registers>
    {
        self.require_stopped()?;
        let thread = self.selected_or_error()?;
        Ok(self.backend.read_registers(thread)?)
    }

    /// Read `thread`'s registers.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error (for an unknown thread,
    /// [`BackendError::NotFound`](crate::backend::BackendError::NotFound)).
    pub fn read_registers_for(&mut self, thread: ThreadId) -> Result<Registers>
    {
        self.require_stopped()?;
        Ok(self.backend.read_registers(thread)?)
    }

    /// Write `registers` to the selected thread.
    ///
    /// # Errors
    ///
    /// As for [`read_registers`](Self::read_registers).
    pub fn write_registers(&mut self, registers: &Registers) -> Result<()>
    {
        self.require_stopped()?;
        let thread = self.selected_or_error()?;
        Ok(self.backend.write_registers(thread, registers)?)
    }

    /// Write `registers` to `thread`.
    ///
    /// # Errors
    ///
    /// As for [`read_registers_for`](Self::read_registers_for).
    pub fn write_registers_for(&mut self, thread: ThreadId, registers: &Registers) -> Result<()>
    {
        self.require_stopped()?;
        Ok(self.backend.write_registers(thread, registers)?)
    }

    /// Read `len` bytes at `address`.
    ///
    /// The bytes are returned as they are in the target, including the trap bytes of
    /// inserted breakpoints (known gap, T07).
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, or the backend's error.
    pub fn read_memory(&mut self, address: Address, len: usize) -> Result<Vec<u8>>
    {
        self.require_stopped()?;
        Ok(self.backend.read_memory(address, len)?)
    }

    /// Write `data` at `address`.
    ///
    /// The write goes straight to the target. Overwriting an inserted breakpoint's trap
    /// bypasses the breakpoint table: removing or disabling that breakpoint later
    /// writes its saved original bytes back over `data`.
    ///
    /// # Errors
    ///
    /// As for [`read_memory`](Self::read_memory).
    pub fn write_memory(&mut self, address: Address, data: &[u8]) -> Result<()>
    {
        self.require_stopped()?;
        Ok(self.backend.write_memory(address, data)?)
    }

    /// List the target's mapped memory regions.
    ///
    /// # Errors
    ///
    /// As for [`read_memory`](Self::read_memory).
    pub fn memory_regions(&mut self) -> Result<Vec<MemoryRegion>>
    {
        self.require_stopped()?;
        Ok(self.backend.memory_regions()?)
    }
}
