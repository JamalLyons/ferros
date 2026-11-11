//! # Backend: the operating-system boundary
//!
//! [`Backend`] is the single interface every operating system implements. It covers
//! *mechanism* only: control a process, read and write its state, and report why it
//! stopped. All *policy* (breakpoint bookkeeping, stepping strategies, symbolication)
//! lives in portable engine code that takes a backend as a parameter. Architecture
//! knowledge (trap bytes, which register is the pc, ...) lives separately in
//! [`crate::arch`].
//!
//! The interface is synchronous and deliberately small. Its sufficiency yardstick is the
//! GDB remote protocol packet set (register and memory read/write, `vCont` continue and
//! step, library list, stop replies) and LLDB's `NativeProcessProtocol`.
//!
//! ## State machine
//!
//! ```text
//!  NoProcess --launch/attach--> Stopped --resume--> Running
//!      ^                         ^   |                 |
//!      |                         |   |   wait returns Some(non-terminal event)
//!      |                         +---|-----------------+
//!      |                             |                 |
//!      +---- kill / detach ----------+-----------------+  (kill also legal while Running)
//!      +---- wait returns Some(Exited | Killed) -------+
//! ```
//!
//! | Call                                                           | NoProcess | Stopped | Running |
//! |----------------------------------------------------------------|-----------|---------|---------|
//! | `launch`, `attach`                                             | yes       | no      | no      |
//! | `resume`                                                       | no        | yes (+) | no      |
//! | `detach`                                                       | no        | yes     | no      |
//! | `threads`, registers, memory, `memory_regions`, `images`      | no        | yes     | no      |
//! | `wait`                                                         | no        | yes (*) | yes     |
//! | `interrupt`                                                    | no        | yes (no-op) | yes |
//! | `kill`                                                         | no        | yes     | yes     |
//! | `architecture`                                                 | yes       | yes     | yes     |
//!
//! (*) see [`Backend::wait`] for draining queued events and its behaviour with nothing pending.
//! (+) only once every queued event has been drained with [`Backend::wait`].
//!
//! An illegal call returns [`BackendError::InvalidState`].
//!
//! ## Errors never change the state
//!
//! The state changes only when `launch`, `attach`, `resume`, `detach` or `kill` returns
//! `Ok`, or when `wait` returns an event. A call that returns an error, whichever
//! variant, leaves the backend in the state it was in:
//!
//! - `resume` is all or nothing. If the OS fails part-way, the backend stops the threads
//!   it had already resumed before returning the error, and the target is still *Stopped*.
//!   If one of those threads produced a stop in the meantime, that event is queued for
//!   `wait` rather than lost.
//! - If the process dies on its own while *Stopped* (for example it is killed from
//!   outside), calls may start failing with [`BackendError::NotFound`] or
//!   [`BackendError::Os`], but the backend stays *Stopped*. The backend queues the
//!   terminal event as a pending event, so the next `wait` returns it and moves the
//!   backend to *NoProcess*; until then `resume` fails because an event is pending. The
//!   engine can also call `kill` instead.
//! - `kill` on a process that is already dead succeeds and discards the terminal event.
//!
//! ## A stop ends the resume before it
//!
//! When `wait` reports a stop, every request of the preceding `resume` is finished. A
//! `Step` that did not complete (the thread took a signal instead, or another thread
//! stopped first) is cancelled, and an `interrupt` that lost a race against another event
//! is consumed. No `SingleStep` or `Interrupted` event belonging to an earlier resume is
//! ever reported after a later `resume`; the engine asks again if it still wants one.
//!
//! ## Thread identity
//!
//! A [`ThreadId`] names the same thread for as long as that thread lives, across stops
//! and resumes. An id may be reused only after its thread has exited.
//!
//! ## Threading
//!
//! A backend is owned by one engine thread at a time. It is [`Send`] so it can be moved
//! to that thread, but not required to be [`Sync`]. Some operating systems (Linux
//! `ptrace`) only accept requests from the thread that launched or attached, so the
//! engine must issue every call from `launch`/`attach` onwards, including that call
//! itself, on one thread, and move the backend to that thread beforehand. To interrupt
//! the target on behalf of another thread (a UI, a Ctrl-C handler), the engine thread polls
//! [`Backend::wait`] with a finite timeout and calls [`Backend::interrupt`] between polls.
//!
//! ## Breakpoints belong to the engine
//!
//! The engine, not the backend, owns breakpoint bytes. A backend does not insert, remove,
//! step over, or hide trap instructions and keeps no breakpoint table: the engine plants
//! traps with [`Backend::write_memory`] (using [`crate::arch::Arch::trap_instruction`]),
//! [`Backend::read_memory`] returns memory exactly as it is including those traps, and a
//! trap stop is reported as [`StopEvent::Breakpoint`] with the raw, unadjusted pc.
//!
//! ## Out of scope
//!
//! Hardware breakpoints and watchpoints, and asynchronous APIs, are not part of this
//! interface yet.
//!
//! ## Testing without a process
//!
//! `mock::MockBackend` is a scripted, in-memory backend for unit-testing engine code on
//! any machine. It is compiled for this crate's tests, and for other crates when they
//! enable the `test-util` feature of `ferros-core`.

#![forbid(unsafe_code)]

mod error;
#[cfg(any(test, feature = "test-util"))]
pub mod mock;
mod types;

use std::time::Duration;

pub use error::{BackendError, BackendResult};
pub use types::{Environment, LaunchSpec, LoadedImage, ResumeRequest, StdioSpec, StopEvent, ThreadAction};

use crate::types::{Address, Architecture, MemoryRegion, ProcessId, Registers, ThreadId};

/// Operating-system mechanism for controlling one target process.
///
/// See the [module documentation](self) for the state machine, threading rules and the
/// division of labour with the engine. Every method other than
/// [`architecture`](Self::architecture) may also return [`BackendError::Os`] for an
/// unexpected operating-system failure, and every method that names a process, thread or
/// address may return [`BackendError::NotFound`] if the target vanished underneath it.
pub trait Backend: Send
{
    /// Start `spec.program`, stopped before its first instruction.
    ///
    /// No stop event is queued for this initial stop; afterwards the backend is in the
    /// *Stopped* state.
    ///
    /// # Preconditions
    ///
    /// No process (state *NoProcess*).
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] if a process is already being debugged.
    /// - [`BackendError::NotFound`] if the program, working directory, or a stdio path
    ///   does not exist.
    /// - [`BackendError::PermissionDenied`] if the program may not be executed or debugged.
    /// - [`BackendError::Unsupported`] if the program's format or architecture cannot be
    ///   debugged by this backend, or a requested launch option is not supported.
    /// - [`BackendError::Os`] for any other spawn failure.
    fn launch(&mut self, spec: &LaunchSpec) -> BackendResult<ProcessId>;

    /// Attach to the running process `pid` and stop it.
    ///
    /// No stop event is queued for the attach stop; afterwards the backend is in the
    /// *Stopped* state.
    ///
    /// # Preconditions
    ///
    /// No process (state *NoProcess*).
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] if a process is already being debugged.
    /// - [`BackendError::NotFound`] if no process `pid` exists.
    /// - [`BackendError::PermissionDenied`] if the OS forbids debugging it.
    /// - [`BackendError::Os`] for any other failure.
    fn attach(&mut self, pid: ProcessId) -> BackendResult<()>;

    /// Release the target, which keeps running afterwards; the backend returns to
    /// *NoProcess*.
    ///
    /// Stop events still queued for [`wait`](Self::wait) are discarded, and so are
    /// signals the backend is holding (see [`ResumeRequest::signals`]).
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*. The engine must already have removed every trap it wrote,
    /// since the backend does not know about them.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process, or while the target is running.
    /// - [`BackendError::NotFound`] if the process has already died; the backend stays
    ///   *Stopped* until `wait` returns the terminal event or `kill` is called.
    /// - [`BackendError::Os`] if the OS refuses to detach.
    fn detach(&mut self) -> BackendResult<()>;

    /// Terminate and reap the target; the backend returns to *NoProcess*.
    ///
    /// No terminal [`StopEvent`] is reported for a kill, and stop events still queued for
    /// [`wait`](Self::wait) are discarded. Killing a process that has already died
    /// succeeds.
    ///
    /// # Preconditions
    ///
    /// A process exists (either *Stopped* or *Running*).
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process.
    /// - [`BackendError::PermissionDenied`] if the OS refuses to terminate it.
    /// - [`BackendError::Os`] for any other failure.
    fn kill(&mut self) -> BackendResult<()>;

    /// Block until the target stops and return exactly one event.
    ///
    /// - `timeout = Some(d)`: returns `Ok(None)` if nothing happened within `d` (the target
    ///   is still running); `Some(Duration::ZERO)` polls without blocking.
    /// - `timeout = None`: blocks indefinitely.
    ///
    /// When it returns `Some(event)`, every thread of the target is stopped (state
    /// *Stopped*, or *NoProcess* if [`StopEvent::is_terminal`]). If several threads stopped
    /// at once, the extra events are queued and returned by subsequent `wait` calls before
    /// the target runs again: after an event, call `wait(Some(Duration::ZERO))` repeatedly
    /// to drain them; `Ok(None)` then means none is pending.
    ///
    /// While the target is *Stopped* with nothing pending, a finite timeout returns
    /// `Ok(None)` after at most that timeout (possibly immediately). A process that died
    /// while *Stopped* counts as a pending terminal event: `wait` checks for it and
    /// returns it.
    ///
    /// # Preconditions
    ///
    /// A process exists. With `timeout = None` the target must be *Running* or have a
    /// pending event.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process, or with `timeout = None` while
    ///   the target is stopped with nothing pending (it would block forever).
    /// - [`BackendError::Os`] if waiting on the OS fails.
    fn wait(&mut self, timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>;

    /// Resume the target as described by `request`; the backend becomes *Running*.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped* and no event is pending from [`wait`](Self::wait).
    /// Threads named in `request` should come from [`threads`](Self::threads).
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process, while running, or while stop
    ///   events are still pending.
    /// - [`BackendError::NotFound`] if a thread named in `request` does not exist.
    /// - [`BackendError::Unsupported`] if the combination of thread actions cannot be
    ///   expressed on this OS, a signal is named for a thread whose action is `Suspend`,
    ///   or a signal cannot be delivered (see [`ResumeRequest::signals`]).
    /// - [`BackendError::Os`] if the OS fails to resume a thread; the target is left
    ///   *Stopped*.
    fn resume(&mut self, request: &ResumeRequest) -> BackendResult<()>;

    /// Ask a running target to stop; returns immediately.
    ///
    /// The stop is reported by a later [`wait`](Self::wait) as
    /// [`StopEvent::Interrupted`], or as another event if one raced it. The event names a
    /// thread only if the OS attributes the stop to one. Returns `Ok(())`
    /// without doing anything if the target is already stopped.
    ///
    /// # Preconditions
    ///
    /// A process exists.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process.
    /// - [`BackendError::Os`] if the OS fails to deliver the stop request.
    fn interrupt(&mut self) -> BackendResult<()>;

    /// List the target's threads.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::Os`] if enumeration fails.
    fn threads(&mut self) -> BackendResult<Vec<ThreadId>>;

    /// Read the general-purpose (and available vector/floating) registers of `thread`.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*; `thread` belongs to it.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::NotFound`] if `thread` does not exist.
    /// - [`BackendError::Os`] if the OS fails to read the thread state.
    fn read_registers(&mut self, thread: ThreadId) -> BackendResult<Registers>;

    /// Write `registers` to `thread`.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*; `thread` belongs to it; `registers` has the target's
    /// architecture and layout (normally obtained from [`read_registers`](Self::read_registers)).
    /// Where `Registers` holds a value twice (arm64 keeps x29 in both `fp` and
    /// `general[29]`), the named field (`pc`, `sp`, `fp`) is the one written.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::NotFound`] if `thread` does not exist.
    /// - [`BackendError::Unsupported`] if `registers` has a different architecture or a
    ///   layout the backend cannot write.
    /// - [`BackendError::Os`] if the OS fails to write the thread state.
    fn write_registers(&mut self, thread: ThreadId, registers: &Registers) -> BackendResult<()>;

    /// Read `len` bytes at `address`, all or nothing.
    ///
    /// Bytes are returned exactly as they are in the target, including any trap bytes
    /// the engine wrote; the backend never hides or patches them.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::NotFound`] if any byte of the range is unmapped.
    /// - [`BackendError::PermissionDenied`] if the range is mapped but cannot be read.
    /// - [`BackendError::Os`] for any other failure.
    fn read_memory(&mut self, address: Address, len: usize) -> BackendResult<Vec<u8>>;

    /// Write `data` at `address`, all or nothing.
    ///
    /// Must succeed on mapped pages that are not writable (such as code): the backend
    /// temporarily adjusts and then restores the protection, because the engine uses this
    /// call to plant trap instructions.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::NotFound`] if any byte of the range is unmapped.
    /// - [`BackendError::PermissionDenied`] if the OS refuses to make the range writable.
    /// - [`BackendError::Os`] for any other failure.
    fn write_memory(&mut self, address: Address, data: &[u8]) -> BackendResult<()>;

    /// List the target's mapped memory regions.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::Os`] if enumeration fails.
    fn memory_regions(&mut self) -> BackendResult<Vec<MemoryRegion>>;

    /// List the main executable and loaded libraries with their runtime load addresses.
    ///
    /// # Preconditions
    ///
    /// The target is *Stopped*.
    ///
    /// # Errors
    ///
    /// - [`BackendError::InvalidState`] with no process or while running.
    /// - [`BackendError::Unsupported`] if the OS gives no way to enumerate images.
    /// - [`BackendError::Os`] if enumeration fails.
    fn images(&mut self) -> BackendResult<Vec<LoadedImage>>;

    /// Architecture of the target, or of the host before a target exists.
    ///
    /// # Preconditions
    ///
    /// None; legal in every state.
    ///
    /// # Errors
    ///
    /// None; this method is infallible.
    fn architecture(&self) -> Architecture;
}

#[cfg(test)]
mod tests
{
    use super::*;

    /// Minimal backend that only tracks whether a process exists.
    #[derive(Default)]
    struct StubBackend
    {
        pid: Option<ProcessId>,
    }

    impl StubBackend
    {
        fn require_process(&self) -> BackendResult<()>
        {
            self.pid
                .map(|_| ())
                .ok_or_else(|| BackendError::InvalidState("no process".into()))
        }
    }

    impl Backend for StubBackend
    {
        fn launch(&mut self, _spec: &LaunchSpec) -> BackendResult<ProcessId>
        {
            let pid = ProcessId(1);
            self.pid = Some(pid);
            Ok(pid)
        }

        fn attach(&mut self, pid: ProcessId) -> BackendResult<()>
        {
            self.pid = Some(pid);
            Ok(())
        }

        fn detach(&mut self) -> BackendResult<()>
        {
            self.require_process()?;
            self.pid = None;
            Ok(())
        }

        fn kill(&mut self) -> BackendResult<()>
        {
            self.detach()
        }

        fn wait(&mut self, _timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>
        {
            self.require_process()?;
            Ok(None)
        }

        fn resume(&mut self, _request: &ResumeRequest) -> BackendResult<()>
        {
            self.require_process()
        }

        fn interrupt(&mut self) -> BackendResult<()>
        {
            self.require_process()
        }

        fn threads(&mut self) -> BackendResult<Vec<ThreadId>>
        {
            self.require_process()?;
            Ok(vec![ThreadId(1)])
        }

        fn read_registers(&mut self, _thread: ThreadId) -> BackendResult<Registers>
        {
            self.require_process()?;
            Ok(Registers::new().with_arch(Architecture::Arm64))
        }

        fn write_registers(&mut self, _thread: ThreadId, _registers: &Registers) -> BackendResult<()>
        {
            self.require_process()
        }

        fn read_memory(&mut self, _address: Address, len: usize) -> BackendResult<Vec<u8>>
        {
            self.require_process()?;
            Ok(vec![0; len])
        }

        fn write_memory(&mut self, _address: Address, _data: &[u8]) -> BackendResult<()>
        {
            self.require_process()
        }

        fn memory_regions(&mut self) -> BackendResult<Vec<MemoryRegion>>
        {
            self.require_process()?;
            Ok(Vec::new())
        }

        fn images(&mut self) -> BackendResult<Vec<LoadedImage>>
        {
            self.require_process()?;
            Ok(Vec::new())
        }

        fn architecture(&self) -> Architecture
        {
            Architecture::Arm64
        }
    }

    #[test]
    fn boxed_backend_is_object_safe_and_send()
    {
        let mut backend: Box<dyn Backend> = Box::new(StubBackend::default());
        assert_eq!(backend.threads(), Err(BackendError::InvalidState("no process".into())));

        let handle = std::thread::spawn(move || {
            let pid = backend.launch(&LaunchSpec::new("/bin/true"))?;
            let threads = backend.threads()?;
            backend.kill()?;
            Ok::<_, BackendError>((pid, threads, backend.architecture()))
        });
        let (pid, threads, arch) = handle.join().expect("thread panicked").expect("backend failed");
        assert_eq!(pid, ProcessId(1));
        assert_eq!(threads, vec![ThreadId(1)]);
        assert_eq!(arch, Architecture::Arm64);
    }

    #[test]
    fn resume_request_action_for_uses_last_override_then_default()
    {
        let request = ResumeRequest::continue_all()
            .with_thread(ThreadId(2), ThreadAction::Step)
            .with_thread(ThreadId(2), ThreadAction::Suspend)
            .with_signal(ThreadId(2), 2)
            .with_signal(ThreadId(2), 11);
        assert_eq!(request.action_for(ThreadId(1)), ThreadAction::Continue);
        assert_eq!(request.action_for(ThreadId(2)), ThreadAction::Suspend);
        assert_eq!(request.signal_for(ThreadId(2)), Some(11));
        assert_eq!(request.signal_for(ThreadId(1)), None);
        assert_eq!(
            ResumeRequest::all(ThreadAction::Suspend).action_for(ThreadId(9)),
            ThreadAction::Suspend
        );
    }

    #[test]
    fn resume_request_step_suspends_other_threads()
    {
        let request = ResumeRequest::step(ThreadId(7));
        assert_eq!(request.action_for(ThreadId(7)), ThreadAction::Step);
        assert_eq!(request.action_for(ThreadId(8)), ThreadAction::Suspend);
        assert_eq!(request.default_action, ThreadAction::Suspend);
        assert!(request.signals.is_empty());
    }

    #[test]
    fn stop_event_thread_and_terminal()
    {
        let t = ThreadId(3);
        let cases = [
            (
                StopEvent::Breakpoint {
                    thread: t,
                    pc: Address::new(0x1000),
                },
                Some(t),
                false,
            ),
            (StopEvent::SingleStep { thread: t }, Some(t), false),
            (StopEvent::Signal { thread: t, signal: 11 }, Some(t), false),
            (StopEvent::Interrupted { thread: Some(t) }, Some(t), false),
            (StopEvent::Interrupted { thread: None }, None, false),
            (
                StopEvent::Other {
                    thread: Some(t),
                    reason: "x".into(),
                },
                Some(t),
                false,
            ),
            (StopEvent::Exited { code: 0 }, None, true),
            (StopEvent::Killed { signal: 9 }, None, true),
        ];
        for (event, thread, terminal) in cases {
            assert_eq!(event.thread(), thread, "{event:?}");
            assert_eq!(event.is_terminal(), terminal, "{event:?}");
        }
    }

    #[test]
    fn launch_spec_defaults_and_builders()
    {
        let spec = LaunchSpec::new("/bin/prog");
        assert_eq!(spec.program, std::path::PathBuf::from("/bin/prog"));
        assert!(spec.args.is_empty());
        assert_eq!(
            spec.env,
            Environment {
                inherit: true,
                vars: Vec::new()
            }
        );
        assert_eq!(spec.cwd, None);
        assert_eq!(spec.stdin, StdioSpec::Inherit);
        assert_eq!(spec.stdout, StdioSpec::Inherit);
        assert_eq!(spec.stderr, StdioSpec::Inherit);
        assert_eq!(StdioSpec::default(), StdioSpec::Inherit);

        let spec = spec
            .arg("a")
            .args(["b", "c"])
            .inherit_env(false)
            .env("K", "V")
            .cwd("/tmp")
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Path("/tmp/out".into()))
            .stderr(StdioSpec::Null);
        assert_eq!(spec.args, vec!["a", "b", "c"]);
        assert!(!spec.env.inherit);
        assert_eq!(spec.env.vars, vec![("K".to_string(), "V".to_string())]);
        assert_eq!(spec.cwd, Some("/tmp".into()));
        assert_eq!(spec.stdout, StdioSpec::Path("/tmp/out".into()));
    }
}
