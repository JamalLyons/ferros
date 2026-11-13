//! # An in-memory [`Backend`] for testing engine code
//!
//! [`MockBackend`] pretends to be one debugged process: it has a memory map, threads
//! with registers, loaded images, and a *script* of the stops the "process" will make.
//! It follows the state machine and error contract documented on [`Backend`] strictly
//! (the one deliberate difference is that `wait` never blocks, see below),
//! so engine code (breakpoints, stepping) can be unit-tested on any machine without a
//! real process.
//!
//! The module exists in this crate's own tests and, for other crates, behind the
//! `test-util` feature of `ferros-core`.
//!
//! ## How the script works
//!
//! The test enqueues [`MockStop`] values with [`MockBackend::script`]. Each one is one
//! stop: a [`StopEvent`] plus optional side effects (move a thread's pc, replace its
//! registers, add or remove a thread, change memory) that are applied at the moment the
//! stop is delivered. Every `resume` followed by `wait` consumes the next script entry,
//! whatever the [`ResumeRequest`] asked for: the mock does not execute instructions.
//! To check that the engine asked for the right resume, give the stop an expectation with
//! [`MockStop::expect_resume`]; a different request makes `resume` fail.
//!
//! [`MockBackend::script_simultaneous`] enqueues several stops that happen at once (for
//! example two threads hitting breakpoints): the first `wait` returns the first event and
//! the following `wait` calls return the others, as the contract describes.
//!
//! When the target is running and the script is empty, `wait(Some(_))` returns
//! `Ok(None)` immediately (it never sleeps, so tests stay fast) and `wait(None)` returns
//! [`BackendError::InvalidState`] (a real backend would block forever; the mock refuses
//! rather than hang the test).
//!
//! ## Inspecting the mock afterwards
//!
//! `MockBackend` is a cheap handle: [`Clone`] gives a second handle to the *same* mock.
//! A test keeps one clone and hands the other to the engine (as a `Box<dyn Backend>` or a
//! generic `B: Backend`), then inspects the recorded calls, memory and registers through
//! its own clone. Inspection helpers ([`MockBackend::calls`], [`MockBackend::peek_memory`],
//! [`MockBackend::peek_registers`], [`MockBackend::state`], ...) never record a call.
//!
//! ## Example
//!
//! ```ignore
//! use ferros_core::backend::mock::{MockBackend, MockCall, MockStop, arm64_registers};
//! use ferros_core::backend::{Backend, LaunchSpec, ResumeRequest, StopEvent};
//! use ferros_core::types::{Address, ThreadId};
//!
//! let thread = ThreadId(1);
//! let mock = MockBackend::new()
//!     .with_thread(thread, arm64_registers())
//!     .with_region(Address::new(0x1000), "r-x", vec![0; 16]);
//! mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));
//!
//! let mut backend: Box<dyn Backend> = Box::new(mock.clone());
//! backend.launch(&LaunchSpec::new("/bin/prog")).unwrap();
//! backend.resume(&ResumeRequest::continue_all()).unwrap();
//! assert_eq!(backend.wait(None).unwrap(), Some(StopEvent::Exited { code: 0 }));
//!
//! assert_eq!(mock.calls().len(), 3);
//! assert!(matches!(mock.calls()[0], MockCall::Launch(_)));
//! ```

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{Backend, BackendError, BackendResult, LaunchSpec, LoadedImage, ResumeRequest, StopEvent, ThreadAction};
use crate::types::{Address, Architecture, MemoryRegion, MemoryRegionId, ProcessId, Registers, ThreadId};

/// Arm64 register set with every register zero and the 31 general registers (x0-x30)
/// present, ready to pass to [`MockBackend::with_thread`].
#[must_use]
pub fn arm64_registers() -> Registers
{
    let mut registers = Registers::new().with_arch(Architecture::Arm64);
    registers.general = vec![0; 31];
    registers
}

/// Which state of the [`Backend`] state machine the mock is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockState
{
    /// No process: before `launch`/`attach`, or after `kill`, `detach` or a terminal event.
    NoProcess,
    /// The process exists and is stopped.
    Stopped,
    /// The process has been resumed and has not reported a stop yet.
    Running,
}

/// A [`Registers`] value that can be compared with `==`.
///
/// [`Registers`] does not implement [`PartialEq`], so [`MockCall::WriteRegisters`] stores
/// it in this wrapper. Two values are equal when every register, the floating-point state
/// and the architecture are equal.
#[derive(Debug, Clone)]
pub struct MockRegisters(pub Registers);

impl PartialEq for MockRegisters
{
    fn eq(&self, other: &Self) -> bool
    {
        let (a, b) = (&self.0, &other.0);
        a.pc == b.pc
            && a.sp == b.sp
            && a.fp == b.fp
            && a.general == b.general
            && a.status == b.status
            && a.vector == b.vector
            && a.floating == b.floating
            && a.architecture() == b.architecture()
    }
}

impl Eq for MockRegisters {}

impl From<Registers> for MockRegisters
{
    fn from(registers: Registers) -> Self
    {
        Self(registers)
    }
}

/// One recorded [`Backend`] method call with its arguments.
///
/// Every call is recorded, including calls that fail. `architecture()` is not recorded
/// (it takes `&self` and is infallible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockCall
{
    /// [`Backend::launch`].
    Launch(LaunchSpec),
    /// [`Backend::attach`].
    Attach(ProcessId),
    /// [`Backend::detach`].
    Detach,
    /// [`Backend::kill`].
    Kill,
    /// [`Backend::wait`] with its timeout.
    Wait(Option<Duration>),
    /// [`Backend::resume`].
    Resume(ResumeRequest),
    /// [`Backend::interrupt`].
    Interrupt,
    /// [`Backend::threads`].
    Threads,
    /// [`Backend::read_registers`].
    ReadRegisters(ThreadId),
    /// [`Backend::write_registers`].
    WriteRegisters
    {
        /// Thread written.
        thread: ThreadId,
        /// Registers passed in.
        registers: MockRegisters,
    },
    /// [`Backend::read_memory`].
    ReadMemory
    {
        /// Start address.
        address: Address,
        /// Number of bytes requested.
        len: usize,
    },
    /// [`Backend::write_memory`].
    WriteMemory
    {
        /// Start address.
        address: Address,
        /// Bytes passed in.
        data: Vec<u8>,
    },
    /// [`Backend::memory_regions`].
    MemoryRegions,
    /// [`Backend::images`].
    Images,
}

impl MockCall
{
    /// The kind of this call, without its arguments.
    #[must_use]
    pub fn kind(&self) -> MockCallKind
    {
        match self {
            Self::Launch(_) => MockCallKind::Launch,
            Self::Attach(_) => MockCallKind::Attach,
            Self::Detach => MockCallKind::Detach,
            Self::Kill => MockCallKind::Kill,
            Self::Wait(_) => MockCallKind::Wait,
            Self::Resume(_) => MockCallKind::Resume,
            Self::Interrupt => MockCallKind::Interrupt,
            Self::Threads => MockCallKind::Threads,
            Self::ReadRegisters(_) => MockCallKind::ReadRegisters,
            Self::WriteRegisters { .. } => MockCallKind::WriteRegisters,
            Self::ReadMemory { .. } => MockCallKind::ReadMemory,
            Self::WriteMemory { .. } => MockCallKind::WriteMemory,
            Self::MemoryRegions => MockCallKind::MemoryRegions,
            Self::Images => MockCallKind::Images,
        }
    }
}

/// The kind of a [`MockCall`], used to choose which call [`MockBackend::fail_next`] breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MockCallKind
{
    /// [`Backend::launch`].
    Launch,
    /// [`Backend::attach`].
    Attach,
    /// [`Backend::detach`].
    Detach,
    /// [`Backend::kill`].
    Kill,
    /// [`Backend::wait`].
    Wait,
    /// [`Backend::resume`].
    Resume,
    /// [`Backend::interrupt`].
    Interrupt,
    /// [`Backend::threads`].
    Threads,
    /// [`Backend::read_registers`].
    ReadRegisters,
    /// [`Backend::write_registers`].
    WriteRegisters,
    /// [`Backend::read_memory`].
    ReadMemory,
    /// [`Backend::write_memory`].
    WriteMemory,
    /// [`Backend::memory_regions`].
    MemoryRegions,
    /// [`Backend::images`].
    Images,
}

/// A change to the mock process applied when a [`MockStop`] is delivered.
#[derive(Debug, Clone)]
enum Effect
{
    SetPc(ThreadId, Address),
    SetRegisters(ThreadId, Registers),
    AddThread(ThreadId, Registers),
    RemoveThread(ThreadId),
    WriteMemory(Address, Vec<u8>),
}

/// One scripted stop: the event `wait` returns, plus what changed in the process.
///
/// Effects are applied in the order they were added, at the moment the stop is
/// delivered by `wait` (for simultaneous stops, the effects of the whole group are applied
/// when the first of them is delivered, since the whole process stopped at once). If an
/// effect is impossible (for example it names an unknown thread), `wait` returns
/// [`BackendError::InvalidState`] describing the script error and the stop stays queued.
#[derive(Debug, Clone)]
pub struct MockStop
{
    event: StopEvent,
    effects: Vec<Effect>,
    expected_resume: Option<ResumeRequest>,
}

impl MockStop
{
    /// A stop that reports `event` and changes nothing else.
    #[must_use]
    pub fn new(event: StopEvent) -> Self
    {
        Self {
            event,
            effects: Vec::new(),
            expected_resume: None,
        }
    }

    /// The event `wait` returns for this stop.
    #[must_use]
    pub fn event(&self) -> &StopEvent
    {
        &self.event
    }

    /// Set `thread`'s pc when the stop is delivered.
    #[must_use]
    pub fn set_pc(mut self, thread: ThreadId, pc: Address) -> Self
    {
        self.effects.push(Effect::SetPc(thread, pc));
        self
    }

    /// Replace `thread`'s registers when the stop is delivered.
    #[must_use]
    pub fn set_registers(mut self, thread: ThreadId, registers: Registers) -> Self
    {
        self.effects.push(Effect::SetRegisters(thread, registers));
        self
    }

    /// Create a new thread (which must not exist yet) when the stop is delivered.
    #[must_use]
    pub fn add_thread(mut self, thread: ThreadId, registers: Registers) -> Self
    {
        self.effects.push(Effect::AddThread(thread, registers));
        self
    }

    /// Remove an existing thread when the stop is delivered.
    #[must_use]
    pub fn remove_thread(mut self, thread: ThreadId) -> Self
    {
        self.effects.push(Effect::RemoveThread(thread));
        self
    }

    /// Overwrite mapped memory when the stop is delivered (the process wrote it).
    #[must_use]
    pub fn write_memory(mut self, address: Address, data: impl Into<Vec<u8>>) -> Self
    {
        self.effects.push(Effect::WriteMemory(address, data.into()));
        self
    }

    /// Require the `resume` that leads to this stop to be exactly `request`.
    ///
    /// A different request makes `resume` return [`BackendError::InvalidState`] naming
    /// both requests; the mock stays *Stopped* and the script is unchanged.
    #[must_use]
    pub fn expect_resume(mut self, request: ResumeRequest) -> Self
    {
        self.expected_resume = Some(request);
        self
    }
}

#[derive(Debug, Clone)]
struct Region
{
    permissions: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct Inner
{
    state: MockState,
    pid: ProcessId,
    architecture: Architecture,
    threads: BTreeMap<u64, Registers>,
    memory: BTreeMap<u64, Region>,
    images: Vec<LoadedImage>,
    /// One entry per future resume; each entry is one or more simultaneous stops.
    script: VecDeque<Vec<MockStop>>,
    /// Events of the last stop not yet returned by `wait`.
    pending: VecDeque<StopEvent>,
    interrupt_requested: bool,
    calls: Vec<MockCall>,
    failures: VecDeque<(MockCallKind, BackendError)>,
}

/// In-memory, scripted implementation of [`Backend`]. See the [module documentation](self).
///
/// Cloning gives another handle to the same mock (shared state), so a test can keep one
/// handle for assertions while the engine owns the other.
#[derive(Debug, Clone)]
pub struct MockBackend
{
    inner: Arc<Mutex<Inner>>,
}

impl Default for MockBackend
{
    fn default() -> Self
    {
        Self::new()
    }
}

fn invalid_state(message: impl Into<String>) -> BackendError
{
    BackendError::InvalidState(message.into())
}

fn unmapped(address: u64) -> BackendError
{
    BackendError::NotFound(format!("address {address:#x} is not mapped"))
}

fn unknown_thread(thread: ThreadId) -> BackendError
{
    BackendError::NotFound(format!("thread {} does not exist", thread.0))
}

/// Split `[address, address + len)` into `(region start, offset, length)` pieces, or fail
/// with `NotFound` if any byte is unmapped.
fn segments(memory: &BTreeMap<u64, Region>, address: Address, len: usize) -> BackendResult<Vec<(u64, usize, usize)>>
{
    let mut pieces = Vec::new();
    let mut cursor = address.value();
    let mut remaining = len;
    while remaining > 0 {
        let (&start, region) = memory.range(..=cursor).next_back().ok_or_else(|| unmapped(cursor))?;
        let offset = usize::try_from(cursor - start).map_err(|_| unmapped(cursor))?;
        if offset >= region.bytes.len() {
            return Err(unmapped(cursor));
        }
        let take = remaining.min(region.bytes.len() - offset);
        pieces.push((start, offset, take));
        remaining -= take;
        if remaining > 0 {
            cursor = cursor.checked_add(take as u64).ok_or_else(|| unmapped(u64::MAX))?;
        }
    }
    Ok(pieces)
}

fn read_bytes(memory: &BTreeMap<u64, Region>, address: Address, len: usize) -> BackendResult<Vec<u8>>
{
    // Validate the range before allocating: a bogus `len` must be an error, not a panic.
    let pieces = segments(memory, address, len)?;
    let mut out = Vec::with_capacity(len);
    for (start, offset, take) in pieces {
        out.extend_from_slice(&memory[&start].bytes[offset..offset + take]);
    }
    Ok(out)
}

fn write_bytes(memory: &mut BTreeMap<u64, Region>, address: Address, data: &[u8]) -> BackendResult<()>
{
    let mut written = 0;
    for (start, offset, take) in segments(memory, address, data.len())? {
        if let Some(region) = memory.get_mut(&start) {
            region.bytes[offset..offset + take].copy_from_slice(&data[written..written + take]);
        }
        written += take;
    }
    Ok(())
}

impl Inner
{
    /// Record `call`, then fail if an injected failure matches its kind.
    fn begin(&mut self, call: MockCall) -> BackendResult<()>
    {
        let kind = call.kind();
        self.calls.push(call);
        let index = self.failures.iter().position(|(k, _)| *k == kind);
        match index.and_then(|index| self.failures.remove(index)) {
            Some((_, error)) => Err(error),
            None => Ok(()),
        }
    }

    fn require_stopped(&self, call: &str) -> BackendResult<()>
    {
        match self.state {
            MockState::Stopped => Ok(()),
            MockState::NoProcess => Err(invalid_state(format!("{call}: no process"))),
            MockState::Running => Err(invalid_state(format!("{call}: the target is running"))),
        }
    }

    fn require_process(&self, call: &str) -> BackendResult<()>
    {
        if self.state == MockState::NoProcess {
            Err(invalid_state(format!("{call}: no process")))
        } else {
            Ok(())
        }
    }

    fn end_process(&mut self)
    {
        self.state = MockState::NoProcess;
        self.pending.clear();
        self.interrupt_requested = false;
    }

    /// Deliver an event: terminal events end the process, others leave it stopped.
    fn deliver(&mut self, event: StopEvent) -> StopEvent
    {
        if event.is_terminal() {
            self.end_process();
        } else {
            self.state = MockState::Stopped;
        }
        event
    }

    /// Apply the effects of `stops` to copies of the threads and memory, then commit.
    fn apply_effects(&mut self, stops: &[MockStop]) -> BackendResult<()>
    {
        if stops.iter().all(|stop| stop.effects.is_empty()) {
            return Ok(());
        }
        let mut threads = self.threads.clone();
        let mut memory = self.memory.clone();
        for effect in stops.iter().flat_map(|stop| &stop.effects) {
            let script_error = |what: String| invalid_state(format!("mock script error: {what}"));
            match effect {
                Effect::SetPc(thread, pc) => {
                    threads
                        .get_mut(&thread.0)
                        .ok_or_else(|| script_error(format!("set_pc on unknown thread {}", thread.0)))?
                        .pc = *pc;
                }
                Effect::SetRegisters(thread, registers) => {
                    *threads
                        .get_mut(&thread.0)
                        .ok_or_else(|| script_error(format!("set_registers on unknown thread {}", thread.0)))? =
                        registers.clone();
                }
                Effect::AddThread(thread, registers) => {
                    if threads.insert(thread.0, registers.clone()).is_some() {
                        return Err(script_error(format!("add_thread: thread {} already exists", thread.0)));
                    }
                }
                Effect::RemoveThread(thread) => {
                    threads
                        .remove(&thread.0)
                        .ok_or_else(|| script_error(format!("remove_thread on unknown thread {}", thread.0)))?;
                }
                Effect::WriteMemory(address, data) => {
                    write_bytes(&mut memory, *address, data)
                        .map_err(|error| script_error(format!("write_memory effect: {error}")))?;
                }
            }
        }
        self.threads = threads;
        self.memory = memory;
        Ok(())
    }
}

impl MockBackend
{
    /// An arm64 mock with pid 1, no threads, no memory, no images and an empty script.
    #[must_use]
    pub fn new() -> Self
    {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: MockState::NoProcess,
                pid: ProcessId(1),
                architecture: Architecture::Arm64,
                threads: BTreeMap::new(),
                memory: BTreeMap::new(),
                images: Vec::new(),
                script: VecDeque::new(),
                pending: VecDeque::new(),
                interrupt_requested: false,
                calls: Vec::new(),
                failures: VecDeque::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner>
    {
        // A panic in another test thread must not hide this mock's state.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Pid returned by `launch` (default 1). `attach(pid)` replaces it with `pid`.
    #[must_use]
    pub fn with_pid(self, pid: ProcessId) -> Self
    {
        self.lock().pid = pid;
        self
    }

    /// Architecture reported by `architecture()` (default [`Architecture::Arm64`]).
    /// `write_registers` rejects registers of any other architecture.
    #[must_use]
    pub fn with_architecture(self, architecture: Architecture) -> Self
    {
        self.lock().architecture = architecture;
        self
    }

    /// Add a thread with its initial registers (replacing an existing thread of that id).
    #[must_use]
    pub fn with_thread(self, thread: ThreadId, registers: Registers) -> Self
    {
        self.lock().threads.insert(thread.0, registers);
        self
    }

    /// Map `bytes` at `start` with `permissions` (such as `"r-x"`).
    ///
    /// Adjacent regions may touch; reads and writes then cross between them.
    ///
    /// # Panics
    ///
    /// If `bytes` is empty, the region would extend past the end of the address space,
    /// or it overlaps a region already mapped: these are mistakes in the test setup.
    #[must_use]
    pub fn with_region(self, start: Address, permissions: &str, bytes: impl Into<Vec<u8>>) -> Self
    {
        let bytes = bytes.into();
        {
            let mut inner = self.lock();
            let start = start.value();
            assert!(!bytes.is_empty(), "mock region at {start:#x} is empty");
            let end = start
                .checked_add(bytes.len() as u64)
                .unwrap_or_else(|| panic!("mock region at {start:#x} overflows the address space"));
            let overlaps = inner
                .memory
                .iter()
                .any(|(&s, region)| s < end && start < s + region.bytes.len() as u64);
            assert!(!overlaps, "mock region {start:#x}..{end:#x} overlaps an existing region");
            inner.memory.insert(
                start,
                Region {
                    permissions: permissions.to_owned(),
                    bytes,
                },
            );
        }
        self
    }

    /// Add a loaded image reported by `images()`.
    #[must_use]
    pub fn with_image(self, image: LoadedImage) -> Self
    {
        self.lock().images.push(image);
        self
    }

    /// Replace the live loader image list (for load/unload and relocation tests).
    pub fn set_images(&self, images: Vec<LoadedImage>)
    {
        self.lock().images = images;
    }

    /// Enqueue one stop, delivered by the `wait` after a future `resume`.
    pub fn script(&self, stop: MockStop)
    {
        self.lock().script.push_back(vec![stop]);
    }

    /// Enqueue several stops that happen at once after one future `resume`.
    ///
    /// The first `wait` returns the first event; subsequent `wait` calls return the others
    /// in order, and `resume` is refused until all are drained. Every stop's
    /// [`expect_resume`](MockStop::expect_resume), if set, must match that resume. Put a
    /// terminal event last: once delivered, the process is gone and the rest are dropped.
    /// An empty group is ignored.
    pub fn script_simultaneous(&self, stops: impl IntoIterator<Item = MockStop>)
    {
        let stops: Vec<MockStop> = stops.into_iter().collect();
        if !stops.is_empty() {
            self.lock().script.push_back(stops);
        }
    }

    /// Simulate the process dying on its own (for example killed from outside) while it
    /// is *Stopped*: `event` is queued as a pending terminal event, as the contract
    /// requires of a real backend. The next `wait` returns it and the mock goes to
    /// *NoProcess*; until then `resume` is refused.
    ///
    /// # Panics
    ///
    /// If `event` is not terminal or the mock is not *Stopped* (to script a death while
    /// running, enqueue a terminal [`MockStop`] instead).
    pub fn die(&self, event: StopEvent)
    {
        let mut inner = self.lock();
        assert!(event.is_terminal(), "die: {event:?} is not a terminal event");
        assert_eq!(inner.state, MockState::Stopped, "die: the mock is not stopped");
        inner.pending.push_back(event);
    }

    /// Make the next call of `kind` fail with `error`, after it is recorded and before
    /// anything else happens (the state does not change). Several failures can be queued;
    /// each is used once.
    pub fn fail_next(&self, kind: MockCallKind, error: BackendError)
    {
        self.lock().failures.push_back((kind, error));
    }

    /// Every recorded call so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<MockCall>
    {
        self.lock().calls.clone()
    }

    /// Return the recorded calls and clear the log.
    #[must_use]
    pub fn take_calls(&self) -> Vec<MockCall>
    {
        std::mem::take(&mut self.lock().calls)
    }

    /// Current state of the state machine.
    #[must_use]
    pub fn state(&self) -> MockState
    {
        self.lock().state
    }

    /// Number of scripted stops not yet delivered (simultaneous stops count one each),
    /// excluding events already pending from the last stop.
    #[must_use]
    pub fn script_len(&self) -> usize
    {
        self.lock().script.iter().map(Vec::len).sum()
    }

    /// Number of events of the last stop that `wait` has not returned yet.
    #[must_use]
    pub fn pending_len(&self) -> usize
    {
        self.lock().pending.len()
    }

    /// Read memory directly, in any state, without recording a call.
    ///
    /// # Errors
    ///
    /// [`BackendError::NotFound`] if any byte of the range is unmapped.
    pub fn peek_memory(&self, address: Address, len: usize) -> BackendResult<Vec<u8>>
    {
        read_bytes(&self.lock().memory, address, len)
    }

    /// A thread's registers, in any state, without recording a call; `None` for an
    /// unknown thread.
    #[must_use]
    pub fn peek_registers(&self, thread: ThreadId) -> Option<Registers>
    {
        self.lock().threads.get(&thread.0).cloned()
    }

    /// Thread ids in ascending order, in any state, without recording a call.
    #[must_use]
    pub fn peek_threads(&self) -> Vec<ThreadId>
    {
        self.lock().threads.keys().copied().map(ThreadId).collect()
    }
}

impl Backend for MockBackend
{
    fn launch(&mut self, spec: &LaunchSpec) -> BackendResult<ProcessId>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Launch(spec.clone()))?;
        if inner.state != MockState::NoProcess {
            return Err(invalid_state("launch: a process is already being debugged"));
        }
        inner.state = MockState::Stopped;
        Ok(inner.pid)
    }

    fn attach(&mut self, pid: ProcessId) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Attach(pid))?;
        if inner.state != MockState::NoProcess {
            return Err(invalid_state("attach: a process is already being debugged"));
        }
        inner.pid = pid;
        inner.state = MockState::Stopped;
        Ok(())
    }

    fn detach(&mut self) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Detach)?;
        inner.require_stopped("detach")?;
        inner.end_process();
        Ok(())
    }

    fn kill(&mut self) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Kill)?;
        inner.require_process("kill")?;
        inner.end_process();
        Ok(())
    }

    /// Never sleeps: with nothing to report, a finite timeout returns `Ok(None)` at once.
    fn wait(&mut self, timeout: Option<Duration>) -> BackendResult<Option<StopEvent>>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Wait(timeout))?;
        match inner.state {
            MockState::NoProcess => Err(invalid_state("wait: no process")),
            MockState::Stopped => match inner.pending.pop_front() {
                Some(event) => Ok(Some(inner.deliver(event))),
                None if timeout.is_some() => Ok(None),
                None => Err(invalid_state(
                    "wait(None): the target is stopped with nothing pending and would block forever",
                )),
            },
            MockState::Running => {
                if inner.interrupt_requested {
                    inner.interrupt_requested = false;
                    return Ok(Some(inner.deliver(StopEvent::Interrupted { thread: None })));
                }
                let Some(stops) = inner.script.front().cloned() else {
                    return match timeout {
                        Some(_) => Ok(None),
                        None => Err(invalid_state(
                            "wait(None): the mock script is exhausted; a real backend would block forever",
                        )),
                    };
                };
                inner.apply_effects(&stops)?;
                inner.script.pop_front();
                let mut events = stops.into_iter().map(|stop| stop.event);
                let first = events.next().unwrap_or(StopEvent::Interrupted { thread: None });
                inner.pending.extend(events);
                Ok(Some(inner.deliver(first)))
            }
        }
    }

    fn resume(&mut self, request: &ResumeRequest) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Resume(request.clone()))?;
        inner.require_stopped("resume")?;
        if !inner.pending.is_empty() {
            return Err(invalid_state(format!(
                "resume: {} stop event(s) still pending; drain them with wait first",
                inner.pending.len()
            )));
        }
        if let Some((thread, _)) = request
            .thread_actions
            .iter()
            .find(|(thread, _)| !inner.threads.contains_key(&thread.0))
        {
            return Err(unknown_thread(*thread));
        }
        for (thread, signal) in &request.signals {
            if !inner.threads.contains_key(&thread.0) {
                return Err(unknown_thread(*thread));
            }
            if request.action_for(*thread) == ThreadAction::Suspend {
                return Err(BackendError::Unsupported(format!(
                    "resume: signal {signal} is named for thread {}, which stays suspended",
                    thread.0
                )));
            }
        }
        if let Some(stops) = inner.script.front() {
            for expected in stops.iter().filter_map(|stop| stop.expected_resume.as_ref()) {
                if expected != request {
                    return Err(invalid_state(format!(
                        "resume: the mock script expected {expected:?} but the engine asked for {request:?}"
                    )));
                }
            }
        }
        inner.state = MockState::Running;
        Ok(())
    }

    fn interrupt(&mut self) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Interrupt)?;
        match inner.state {
            MockState::NoProcess => Err(invalid_state("interrupt: no process")),
            MockState::Stopped => Ok(()),
            MockState::Running => {
                inner.interrupt_requested = true;
                Ok(())
            }
        }
    }

    fn threads(&mut self) -> BackendResult<Vec<ThreadId>>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Threads)?;
        inner.require_stopped("threads")?;
        Ok(inner.threads.keys().copied().map(ThreadId).collect())
    }

    fn read_registers(&mut self, thread: ThreadId) -> BackendResult<Registers>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::ReadRegisters(thread))?;
        inner.require_stopped("read_registers")?;
        inner.threads.get(&thread.0).cloned().ok_or_else(|| unknown_thread(thread))
    }

    fn write_registers(&mut self, thread: ThreadId, registers: &Registers) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::WriteRegisters {
            thread,
            registers: registers.clone().into(),
        })?;
        inner.require_stopped("write_registers")?;
        if registers.architecture() != inner.architecture {
            return Err(BackendError::Unsupported(format!(
                "write_registers: registers are for {:?}, the target is {:?}",
                registers.architecture(),
                inner.architecture
            )));
        }
        let is_arm64 = inner.architecture == Architecture::Arm64;
        let slot = inner.threads.get_mut(&thread.0).ok_or_else(|| unknown_thread(thread))?;
        *slot = registers.clone();
        // The contract says the named field wins where arm64 holds x29 twice.
        if is_arm64 && let Some(x29) = slot.general.get_mut(29) {
            *x29 = slot.fp.value();
        }
        Ok(())
    }

    fn read_memory(&mut self, address: Address, len: usize) -> BackendResult<Vec<u8>>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::ReadMemory { address, len })?;
        inner.require_stopped("read_memory")?;
        read_bytes(&inner.memory, address, len)
    }

    /// Succeeds on mapped regions whatever their permissions, as the contract requires.
    fn write_memory(&mut self, address: Address, data: &[u8]) -> BackendResult<()>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::WriteMemory {
            address,
            data: data.to_vec(),
        })?;
        inner.require_stopped("write_memory")?;
        write_bytes(&mut inner.memory, address, data)
    }

    fn memory_regions(&mut self) -> BackendResult<Vec<MemoryRegion>>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::MemoryRegions)?;
        inner.require_stopped("memory_regions")?;
        Ok(inner
            .memory
            .iter()
            .enumerate()
            .map(|(index, (&start, region))| {
                MemoryRegion::new(
                    MemoryRegionId(index),
                    Address::new(start),
                    Address::new(start + region.bytes.len() as u64),
                    region.permissions.clone(),
                    None,
                )
            })
            .collect())
    }

    fn images(&mut self) -> BackendResult<Vec<LoadedImage>>
    {
        let mut inner = self.lock();
        inner.begin(MockCall::Images)?;
        inner.require_stopped("images")?;
        Ok(inner.images.clone())
    }

    fn architecture(&self) -> Architecture
    {
        self.lock().architecture
    }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::arch;

    const T1: ThreadId = ThreadId(1);
    const CODE: u64 = 0x1000;
    const BP: u64 = 0x1008;

    fn regs_at(pc: u64) -> Registers
    {
        let mut registers = arm64_registers();
        registers.pc = Address::new(pc);
        registers
    }

    fn process() -> MockBackend
    {
        MockBackend::new()
            .with_pid(ProcessId(42))
            .with_thread(T1, regs_at(CODE))
            .with_region(Address::new(CODE), "r-x", (0u8..32).collect::<Vec<_>>())
            .with_region(Address::new(CODE + 32), "rw-", vec![0xAA; 16])
            .with_region(Address::new(0x8000), "r--", vec![0x55; 8])
            .with_image(LoadedImage {
                path: "/bin/prog".into(),
                load_address: Address::new(CODE),
            })
    }

    fn is_invalid_state<T: std::fmt::Debug>(result: &BackendResult<T>) -> bool
    {
        matches!(result, Err(BackendError::InvalidState(_)))
    }

    fn spec() -> LaunchSpec
    {
        LaunchSpec::new("/bin/prog")
    }

    /// Drives the mock the way an engine continues past a breakpoint and asserts the
    /// exact call sequence.
    #[test]
    fn launch_breakpoint_step_exit_records_exact_calls()
    {
        let mock = process();
        let arch = arch::for_architecture(mock.architecture()).expect("arm64");
        let trap = arch.trap_instruction().to_vec();
        let bp = Address::new(BP);

        mock.script(
            MockStop::new(StopEvent::Breakpoint { thread: T1, pc: bp })
                .set_pc(T1, bp)
                .expect_resume(ResumeRequest::continue_all()),
        );
        mock.script(
            MockStop::new(StopEvent::SingleStep { thread: T1 })
                .set_pc(T1, Address::new(BP + 4))
                .expect_resume(ResumeRequest::step(T1)),
        );
        mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(ResumeRequest::continue_all()));

        let mut engine = mock.clone();
        let backend: &mut dyn Backend = &mut engine;

        assert_eq!(backend.launch(&spec()), Ok(ProcessId(42)));
        assert_eq!(backend.threads(), Ok(vec![T1]));
        let original = backend.read_memory(bp, trap.len()).expect("read original");
        assert_eq!(original, vec![8, 9, 10, 11]);
        backend.write_memory(bp, &trap).expect("plant trap");
        backend.resume(&ResumeRequest::continue_all()).expect("continue");
        let event = backend.wait(None).expect("wait").expect("event");
        assert_eq!(event, StopEvent::Breakpoint { thread: T1, pc: bp });
        let registers = backend.read_registers(T1).expect("registers");
        let StopEvent::Breakpoint { pc, .. } = event else {
            unreachable!()
        };
        assert_eq!(arch.pc_after_trap(pc), arch.pc(&registers));
        backend.write_memory(bp, &original).expect("restore");
        backend.resume(&ResumeRequest::step(T1)).expect("step");
        assert_eq!(backend.wait(None), Ok(Some(StopEvent::SingleStep { thread: T1 })));
        backend.write_memory(bp, &trap).expect("re-plant trap");
        backend.resume(&ResumeRequest::continue_all()).expect("continue");
        assert_eq!(backend.wait(None), Ok(Some(StopEvent::Exited { code: 0 })));

        assert_eq!(
            mock.take_calls(),
            [
                MockCall::Launch(spec()),
                MockCall::Threads,
                MockCall::ReadMemory { address: bp, len: 4 },
                MockCall::WriteMemory {
                    address: bp,
                    data: trap.clone()
                },
                MockCall::Resume(ResumeRequest::continue_all()),
                MockCall::Wait(None),
                MockCall::ReadRegisters(T1),
                MockCall::WriteMemory {
                    address: bp,
                    data: original
                },
                MockCall::Resume(ResumeRequest::step(T1)),
                MockCall::Wait(None),
                MockCall::WriteMemory {
                    address: bp,
                    data: trap.clone()
                },
                MockCall::Resume(ResumeRequest::continue_all()),
                MockCall::Wait(None),
            ]
        );
        assert!(mock.calls().is_empty());
        assert_eq!(mock.state(), MockState::NoProcess);
        assert_eq!(mock.script_len(), 0);
        assert_eq!(mock.peek_memory(bp, 4), Ok(trap));
        assert_eq!(mock.peek_registers(T1).expect("t1").pc, Address::new(BP + 4));
    }

    #[test]
    fn illegal_calls_return_invalid_state_and_keep_state()
    {
        let mut mock = process();
        let a = Address::new(CODE);

        // NoProcess.
        assert!(is_invalid_state(&mock.detach()));
        assert!(is_invalid_state(&mock.kill()));
        assert!(is_invalid_state(&mock.wait(Some(Duration::ZERO))));
        assert!(is_invalid_state(&mock.resume(&ResumeRequest::continue_all())));
        assert!(is_invalid_state(&mock.interrupt()));
        assert!(is_invalid_state(&mock.threads()));
        assert!(is_invalid_state(&mock.read_registers(T1)));
        assert!(is_invalid_state(&mock.write_registers(T1, &arm64_registers())));
        assert!(is_invalid_state(&mock.read_memory(a, 1)));
        assert!(is_invalid_state(&mock.write_memory(a, &[0])));
        assert!(is_invalid_state(&mock.memory_regions()));
        assert!(is_invalid_state(&mock.images()));
        assert_eq!(mock.state(), MockState::NoProcess);
        assert_eq!(mock.architecture(), Architecture::Arm64);
        assert_eq!(mock.take_calls().len(), 12);

        // Stopped.
        mock.launch(&spec()).expect("launch");
        assert!(is_invalid_state(&mock.launch(&spec())));
        assert!(is_invalid_state(&mock.attach(ProcessId(7))));
        assert!(is_invalid_state(&mock.wait(None)));
        assert_eq!(mock.state(), MockState::Stopped);
        assert_eq!(
            mock.take_calls(),
            [
                MockCall::Launch(spec()),
                MockCall::Launch(spec()),
                MockCall::Attach(ProcessId(7)),
                MockCall::Wait(None)
            ]
        );

        // Running.
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        assert!(is_invalid_state(&mock.launch(&spec())));
        assert!(is_invalid_state(&mock.attach(ProcessId(7))));
        assert!(is_invalid_state(&mock.resume(&ResumeRequest::continue_all())));
        assert!(is_invalid_state(&mock.detach()));
        assert!(is_invalid_state(&mock.threads()));
        assert!(is_invalid_state(&mock.read_registers(T1)));
        assert!(is_invalid_state(&mock.write_registers(T1, &arm64_registers())));
        assert!(is_invalid_state(&mock.read_memory(a, 1)));
        assert!(is_invalid_state(&mock.write_memory(a, &[0])));
        assert!(is_invalid_state(&mock.memory_regions()));
        assert!(is_invalid_state(&mock.images()));
        assert_eq!(mock.state(), MockState::Running);
        assert_eq!(mock.take_calls().len(), 12);
        assert_eq!(mock.peek_memory(a, 1), Ok(vec![0]));
    }

    #[test]
    fn attach_detach_and_kill_return_end_process()
    {
        let mut mock = process();
        mock.attach(ProcessId(99)).expect("attach");
        assert_eq!(mock.state(), MockState::Stopped);
        mock.detach().expect("detach");
        assert_eq!(mock.state(), MockState::NoProcess);
        assert_eq!(mock.launch(&spec()), Ok(ProcessId(99)));
        mock.kill().expect("kill while stopped");
        assert_eq!(mock.state(), MockState::NoProcess);
    }

    #[test]
    fn memory_map_cross_region_unmapped_and_code_writes()
    {
        let mut mock = process();
        mock.launch(&spec()).expect("launch");

        // Read and write across the r-x / rw- boundary at CODE + 32.
        let across = Address::new(CODE + 30);
        assert_eq!(mock.read_memory(across, 4), Ok(vec![30, 31, 0xAA, 0xAA]));
        mock.write_memory(across, &[1, 2, 3, 4]).expect("cross-region write");
        assert_eq!(mock.read_memory(across, 4), Ok(vec![1, 2, 3, 4]));

        // Any unmapped byte fails the whole access, and a failed write changes nothing.
        let tail = Address::new(CODE + 46);
        assert!(matches!(mock.read_memory(tail, 4), Err(BackendError::NotFound(_))));
        assert!(matches!(mock.write_memory(tail, &[9; 4]), Err(BackendError::NotFound(_))));
        assert_eq!(mock.read_memory(tail, 2), Ok(vec![0xAA, 0xAA]));
        assert!(matches!(
            mock.read_memory(Address::new(0x10), 1),
            Err(BackendError::NotFound(_))
        ));
        assert!(matches!(
            mock.read_memory(Address::new(0x8008), 1),
            Err(BackendError::NotFound(_))
        ));

        // Writing into read-only code succeeds (the engine plants traps there).
        mock.write_memory(Address::new(0x8000), &[7]).expect("write r--");
        mock.write_memory(Address::new(CODE), &arch::Arm64::TRAP_INSTRUCTION)
            .expect("write r-x");
        assert_eq!(
            mock.read_memory(Address::new(CODE), 4),
            Ok(arch::Arm64::TRAP_INSTRUCTION.to_vec())
        );
        assert_eq!(mock.read_memory(Address::new(CODE), 0), Ok(Vec::new()));

        let regions = mock.memory_regions().expect("regions");
        let summary: Vec<_> = regions
            .iter()
            .map(|r| (r.id, r.start.value(), r.end.value(), r.permissions.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (MemoryRegionId(0), CODE, CODE + 32, "r-x"),
                (MemoryRegionId(1), CODE + 32, CODE + 48, "rw-"),
                (MemoryRegionId(2), 0x8000, 0x8008, "r--"),
            ]
        );
        assert_eq!(
            mock.images(),
            Ok(vec![LoadedImage {
                path: "/bin/prog".into(),
                load_address: Address::new(CODE)
            }])
        );
    }

    #[test]
    #[should_panic(expected = "overlaps")]
    fn overlapping_regions_are_a_setup_error()
    {
        let _ = MockBackend::new()
            .with_region(Address::new(0x1000), "r-x", vec![0; 16])
            .with_region(Address::new(0x100F), "rw-", vec![0; 1]);
    }

    #[test]
    fn registers_round_trip_and_unknown_thread()
    {
        let mut mock = process();
        mock.launch(&spec()).expect("launch");
        let mut registers = mock.read_registers(T1).expect("read");
        assert_eq!(registers.pc, Address::new(CODE));
        registers.general[0] = 0x1234;
        registers.sp = Address::new(0x7000);
        mock.write_registers(T1, &registers).expect("write");
        assert_eq!(
            MockRegisters(mock.read_registers(T1).expect("read back")),
            MockRegisters(registers.clone())
        );

        let missing = ThreadId(9);
        assert!(matches!(mock.read_registers(missing), Err(BackendError::NotFound(_))));
        assert!(matches!(
            mock.write_registers(missing, &registers),
            Err(BackendError::NotFound(_))
        ));
        assert!(matches!(
            mock.resume(&ResumeRequest::step(missing)),
            Err(BackendError::NotFound(_))
        ));
        assert_eq!(mock.state(), MockState::Stopped);

        let x86 = Registers::new().with_arch(Architecture::X86_64);
        assert!(matches!(mock.write_registers(T1, &x86), Err(BackendError::Unsupported(_))));
    }

    #[test]
    fn wait_without_script_never_blocks()
    {
        let mut mock = process();
        mock.launch(&spec()).expect("launch");
        assert_eq!(mock.wait(Some(Duration::ZERO)), Ok(None));
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        let started = std::time::Instant::now();
        assert_eq!(mock.wait(Some(Duration::from_secs(3600))), Ok(None));
        assert!(started.elapsed() < Duration::from_secs(1));
        let Err(BackendError::InvalidState(message)) = mock.wait(None) else {
            panic!("wait(None) with an empty script must fail")
        };
        assert!(message.contains("exhausted"), "{message}");
        assert_eq!(mock.state(), MockState::Running);
    }

    #[test]
    fn simultaneous_events_drain_in_order_and_block_resume()
    {
        let t2 = ThreadId(2);
        let mut mock = process();
        mock.script_simultaneous([
            MockStop::new(StopEvent::Breakpoint {
                thread: T1,
                pc: Address::new(BP),
            })
            .set_pc(T1, Address::new(BP)),
            MockStop::new(StopEvent::Breakpoint {
                thread: t2,
                pc: Address::new(BP),
            })
            .add_thread(t2, regs_at(BP)),
        ]);
        assert_eq!(mock.script_len(), 2);
        mock.launch(&spec()).expect("launch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");

        let first = mock.wait(Some(Duration::ZERO)).expect("wait");
        assert_eq!(first.and_then(|e| e.thread()), Some(T1));
        assert_eq!(mock.pending_len(), 1);
        // Effects of the whole group are visible as soon as the process stops.
        assert_eq!(mock.threads(), Ok(vec![T1, t2]));
        assert!(is_invalid_state(&mock.resume(&ResumeRequest::continue_all())));
        assert_eq!(mock.state(), MockState::Stopped);

        let second = mock.wait(Some(Duration::ZERO)).expect("wait");
        assert_eq!(second.and_then(|e| e.thread()), Some(t2));
        assert_eq!(mock.wait(Some(Duration::ZERO)), Ok(None));
        mock.resume(&ResumeRequest::continue_all()).expect("resume after drain");
    }

    #[test]
    fn interrupt_jumps_ahead_of_the_script()
    {
        let mut mock = process();
        mock.script(MockStop::new(StopEvent::Exited { code: 3 }));
        mock.launch(&spec()).expect("launch");
        mock.interrupt().expect("interrupt while stopped is a no-op");
        assert_eq!(mock.wait(Some(Duration::ZERO)), Ok(None));

        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        mock.interrupt().expect("interrupt");
        assert_eq!(mock.wait(None), Ok(Some(StopEvent::Interrupted { thread: None })));
        assert_eq!(mock.state(), MockState::Stopped);
        assert_eq!(mock.script_len(), 1);

        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        assert_eq!(mock.wait(None), Ok(Some(StopEvent::Exited { code: 3 })));
        assert_eq!(mock.state(), MockState::NoProcess);
    }

    #[test]
    fn kill_while_running_and_terminal_signal()
    {
        let mut mock = process();
        mock.script(MockStop::new(StopEvent::Killed { signal: 9 }));
        mock.launch(&spec()).expect("launch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        mock.kill().expect("kill while running");
        assert_eq!(mock.state(), MockState::NoProcess);
        assert!(is_invalid_state(&mock.wait(Some(Duration::ZERO))));

        mock.launch(&spec()).expect("relaunch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        assert_eq!(mock.wait(None), Ok(Some(StopEvent::Killed { signal: 9 })));
        assert_eq!(mock.state(), MockState::NoProcess);
    }

    #[test]
    fn expected_resume_mismatch_is_refused()
    {
        let mut mock = process();
        mock.script(MockStop::new(StopEvent::SingleStep { thread: T1 }).expect_resume(ResumeRequest::step(T1)));
        mock.launch(&spec()).expect("launch");
        let Err(BackendError::InvalidState(message)) = mock.resume(&ResumeRequest::continue_all()) else {
            panic!("mismatched resume must fail")
        };
        assert!(message.contains("expected"), "{message}");
        assert_eq!(mock.state(), MockState::Stopped);
        assert_eq!(mock.script_len(), 1);

        let step = ResumeRequest::all(ThreadAction::Suspend).with_thread(T1, ThreadAction::Step);
        mock.resume(&step).expect("equal request");
        assert_eq!(mock.wait(None), Ok(Some(StopEvent::SingleStep { thread: T1 })));
    }

    #[test]
    fn script_errors_leave_the_stop_queued()
    {
        let mut mock = process();
        mock.script(MockStop::new(StopEvent::SingleStep { thread: T1 }).set_pc(ThreadId(5), Address::new(0)));
        mock.launch(&spec()).expect("launch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        assert!(is_invalid_state(&mock.wait(None)));
        assert_eq!(mock.state(), MockState::Running);
        assert_eq!(mock.script_len(), 1);
    }

    #[test]
    fn injected_failures_fire_once_and_keep_state()
    {
        let mut mock = process();
        let denied = BackendError::PermissionDenied("no".into());
        mock.fail_next(MockCallKind::Launch, denied.clone());
        mock.fail_next(
            MockCallKind::ReadMemory,
            BackendError::Os {
                code: 5,
                message: "io".into(),
            },
        );
        assert_eq!(mock.launch(&spec()), Err(denied));
        assert_eq!(mock.state(), MockState::NoProcess);
        mock.launch(&spec()).expect("second launch");
        assert!(matches!(
            mock.read_memory(Address::new(CODE), 1),
            Err(BackendError::Os { code: 5, .. })
        ));
        assert_eq!(mock.read_memory(Address::new(CODE), 1), Ok(vec![0]));
        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn boxed_mock_is_send_and_shares_state()
    {
        let mock = process();
        mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
        let mut backend: Box<dyn Backend> = Box::new(mock.clone());
        std::thread::spawn(move || {
            backend.launch(&spec())?;
            backend.resume(&ResumeRequest::continue_all())?;
            backend.wait(None)
        })
        .join()
        .expect("thread panicked")
        .expect("backend failed");
        assert_eq!(mock.state(), MockState::NoProcess);
        assert_eq!(mock.calls().len(), 3);
    }

    /// The engine steps one thread while another has a signal, then names that signal for
    /// its own thread. The mock does not model held signals; it validates the request and
    /// matches it against the script's expectation.
    #[test]
    fn signal_requests_are_validated_per_thread()
    {
        let t2 = ThreadId(2);
        let mut mock = process().with_thread(t2, regs_at(CODE));
        mock.script_simultaneous([
            MockStop::new(StopEvent::Breakpoint {
                thread: T1,
                pc: Address::new(BP),
            }),
            MockStop::new(StopEvent::Signal { thread: t2, signal: 30 }),
        ]);
        mock.script(MockStop::new(StopEvent::SingleStep { thread: T1 }).expect_resume(ResumeRequest::step(T1)));
        let pass_on = ResumeRequest::continue_all().with_signal(t2, 30);
        mock.script(MockStop::new(StopEvent::Exited { code: 0 }).expect_resume(pass_on.clone()));

        mock.launch(&spec()).expect("launch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        assert_eq!(
            mock.wait(None),
            Ok(Some(StopEvent::Breakpoint {
                thread: T1,
                pc: Address::new(BP)
            }))
        );
        assert_eq!(
            mock.wait(Some(Duration::ZERO)),
            Ok(Some(StopEvent::Signal { thread: t2, signal: 30 }))
        );

        // A signal cannot be named for a thread that stays suspended, or for an unknown one.
        assert!(matches!(
            mock.resume(&ResumeRequest::step(T1).with_signal(t2, 30)),
            Err(BackendError::Unsupported(_))
        ));
        assert!(matches!(
            mock.resume(&ResumeRequest::continue_all().with_signal(ThreadId(9), 30)),
            Err(BackendError::NotFound(_))
        ));
        assert_eq!(mock.state(), MockState::Stopped);

        mock.resume(&ResumeRequest::step(T1)).expect("step with the signal held");
        assert_eq!(mock.wait(None), Ok(Some(StopEvent::SingleStep { thread: T1 })));
        // Continuing without the signal is not what the script expects.
        assert!(is_invalid_state(&mock.resume(&ResumeRequest::continue_all())));
        mock.resume(&pass_on).expect("continue, passing the signal on");
        assert_eq!(mock.wait(None), Ok(Some(StopEvent::Exited { code: 0 })));
    }

    #[test]
    fn detach_discards_pending_events()
    {
        let mut mock = process();
        mock.script_simultaneous([
            MockStop::new(StopEvent::Interrupted { thread: Some(T1) }),
            MockStop::new(StopEvent::Other {
                thread: None,
                reason: "second".into(),
            }),
        ]);
        mock.launch(&spec()).expect("launch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        mock.wait(None).expect("first event");
        assert_eq!(mock.pending_len(), 1);
        mock.detach().expect("detach");
        assert_eq!(mock.state(), MockState::NoProcess);
        assert_eq!(mock.pending_len(), 0);

        mock.launch(&spec()).expect("relaunch");
        assert_eq!(mock.wait(Some(Duration::ZERO)), Ok(None));
    }

    #[test]
    fn script_effects_apply_together_or_not_at_all()
    {
        let t2 = ThreadId(2);
        let mut replacement = regs_at(0x2000);
        replacement.general[5] = 55;
        let mut mock = process().with_thread(t2, regs_at(CODE));
        mock.script(
            MockStop::new(StopEvent::Other {
                thread: Some(T1),
                reason: "effects".into(),
            })
            .set_registers(T1, replacement.clone())
            .remove_thread(t2)
            .write_memory(Address::new(CODE + 32), vec![1, 2, 3]),
        );
        // The first effect is fine, the second names a thread that does not exist.
        mock.script(
            MockStop::new(StopEvent::Interrupted { thread: None })
                .set_pc(T1, Address::new(0x3000))
                .remove_thread(ThreadId(9)),
        );

        mock.launch(&spec()).expect("launch");
        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        mock.wait(None).expect("stop with effects");
        assert_eq!(mock.peek_threads(), vec![T1]);
        assert_eq!(
            mock.peek_registers(T1).map(MockRegisters),
            Some(MockRegisters(replacement.clone()))
        );
        assert_eq!(mock.peek_memory(Address::new(CODE + 32), 4), Ok(vec![1, 2, 3, 0xAA]));

        mock.resume(&ResumeRequest::continue_all()).expect("resume");
        assert!(is_invalid_state(&mock.wait(None)));
        // Nothing of the failed group was applied, and the stop is still queued.
        assert_eq!(mock.peek_registers(T1).map(MockRegisters), Some(MockRegisters(replacement)));
        assert_eq!(mock.state(), MockState::Running);
        assert_eq!(mock.script_len(), 1);
    }

    #[test]
    fn absurd_read_length_is_an_error_not_a_panic()
    {
        let mut mock = process();
        mock.launch(&spec()).expect("launch");
        assert!(matches!(
            mock.read_memory(Address::new(CODE), usize::MAX),
            Err(BackendError::NotFound(_))
        ));
        assert!(matches!(
            mock.peek_memory(Address::new(CODE), usize::MAX),
            Err(BackendError::NotFound(_))
        ));
    }

    #[test]
    fn death_while_stopped_is_a_pending_terminal_event()
    {
        let mut mock = process();
        mock.launch(&spec()).expect("launch");
        mock.die(StopEvent::Killed { signal: 9 });
        assert_eq!(mock.state(), MockState::Stopped);
        assert!(is_invalid_state(&mock.resume(&ResumeRequest::continue_all())));
        assert_eq!(mock.wait(Some(Duration::ZERO)), Ok(Some(StopEvent::Killed { signal: 9 })));
        assert_eq!(mock.state(), MockState::NoProcess);

        // `kill` is the other way out, and discards the terminal event.
        mock.launch(&spec()).expect("relaunch");
        mock.die(StopEvent::Exited { code: 1 });
        mock.kill().expect("kill a dead process");
        assert_eq!(mock.state(), MockState::NoProcess);
        assert_eq!(mock.pending_len(), 0);
    }

    #[test]
    fn write_registers_keeps_arm64_x29_in_step_with_fp()
    {
        let mut mock = process();
        mock.launch(&spec()).expect("launch");
        let mut registers = mock.read_registers(T1).expect("read");
        registers.fp = Address::new(0x7FF0);
        registers.general[29] = 1;
        mock.write_registers(T1, &registers).expect("write");
        let stored = mock.read_registers(T1).expect("read back");
        assert_eq!(stored.fp, Address::new(0x7FF0));
        assert_eq!(stored.general[29], 0x7FF0);
    }
}
