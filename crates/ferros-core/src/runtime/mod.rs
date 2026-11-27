//! # What the target's runtime does, without naming an operating system
//!
//! Some engine behaviour depends on code the operating system and its system libraries
//! put in every process, not on the object-file format and not on process control:
//!
//! - **Non-local exits.** A thread can leave frames without returning from them: the
//!   system unwinder resumes it at a landing pad (a panic caught further out), or a
//!   `longjmp` restores a saved context. The session traps the code through which that
//!   happens ([`session::exits`](crate::session)) so step-out sees the frame leave. Which
//!   library holds that code, and which of its symbols are the entries, is the platform's
//!   knowledge: [`TargetRuntime::exit_sources`].
//! - **Signal handler frames.** The kernel delivers a signal by saving the interrupted
//!   register state on the stack and starting the thread in a trampoline that calls the
//!   handler. How that trampoline is recognized and where the saved state lies is the
//!   platform's knowledge: [`TargetRuntime::signal_context`]. The unwinder keeps the
//!   architecture's part, restoring the registers and checking the result
//!   ([`StackUnwinder`](crate::symbols::unwind::StackUnwinder)).
//! - **System locks.** A language runtime may keep a lock in the operating system's own
//!   lock object, whose layout is private to the system library. Whether such a lock is
//!   held is the platform's knowledge: [`TargetRuntime::system_mutex_state`].
//!
//! [`TargetRuntime`] is that question set. Each platform crate implements it and the
//! session is given one ([`Session::with_runtime`](crate::Session::with_runtime)). It is
//! not part of [`Backend`](crate::Backend), which stays process control, nor of
//! [`ObjectFormat`](crate::objfile::ObjectFormat), which stays container parsing (ADR
//! 0001). A session without one ([`NoRuntime`]) traps no exits and finds no signal
//! frames; everything else works unchanged.

#[cfg(any(test, feature = "test-util"))]
pub mod mock;

use std::fmt;
use std::sync::Arc;

use crate::symbols::unwind::MemoryAccess;
use crate::types::{Address, Architecture};

/// The platform's answers about code its runtime puts in every process.
///
/// See the [module documentation](self). An implementor holds no per-process state and is
/// shared by the session (`Arc<dyn TargetRuntime>`).
pub trait TargetRuntime: Send + Sync
{
    /// Images through which a thread leaves frames without returning from them, and their
    /// entry symbols. The default is none: no exit is trapped.
    fn exit_sources(&self) -> &[ExitSource]
    {
        &[]
    }

    /// The register state a signal interrupted, when `frame` is a caller frame in this
    /// platform's signal trampoline. `None` (the default) for any other frame, or when the
    /// saved state cannot be read or does not look like one.
    ///
    /// The unwinder calls it for every arm64 caller frame before any other unwind rule,
    /// so a frame that is not a trampoline must be rejected cheaply and reliably. A
    /// context with fewer than 29 general registers is ignored.
    fn signal_context(&self, memory: &dyn MemoryAccess, frame: &TrampolineFrame) -> Option<SignalContext>
    {
        let _ = (memory, frame);
        None
    }

    /// Whether the system mutex whose bytes, read from `address` without calling into the
    /// process, are `bytes` is held. The value formatter asks this about the mutex a
    /// standard-library `Mutex` keeps in a separate allocation. `None` (the default) when
    /// the platform does not know the layout or the bytes do not look like one.
    fn system_mutex_state(&self, address: Address, bytes: &[u8]) -> Option<LockState>
    {
        let _ = (address, bytes);
        None
    }
}

/// Whether a lock is held, as [`TargetRuntime::system_mutex_state`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState
{
    /// No thread holds the lock.
    Unlocked,
    /// A thread holds the lock, or it is being handed to a waiting thread.
    Locked,
}

/// The runtime of a session with no platform knowledge: no exit sources and no signal
/// frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRuntime;

impl NoRuntime
{
    /// This runtime as a session holds it.
    #[must_use]
    pub fn shared() -> Arc<dyn TargetRuntime>
    {
        Arc::new(Self)
    }
}

impl TargetRuntime for NoRuntime {}

/// Whether a raw symbol table name (any decoration the format adds kept) is an entry of an
/// [`ExitSource`].
pub type EntryMatcher = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// An image whose code a thread passes through to leave frames without returning, and how
/// to recognize its entries.
///
/// A thread that reaches an entry is single-stepped until it leaves the image above the
/// stack it entered with: it has then landed where the exit goes.
#[derive(Clone)]
pub struct ExitSource
{
    image: String,
    entry: EntryMatcher,
}

impl ExitSource
{
    /// The image whose file name is `image`; a symbol is an entry when `entry` accepts its
    /// raw name.
    pub fn new(image: impl Into<String>, entry: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self
    {
        Self {
            image: image.into(),
            entry: Arc::new(entry),
        }
    }

    /// The image whose file name is `image`, with the entries whose raw names are exactly
    /// `names`.
    #[must_use]
    pub fn named(image: impl Into<String>, names: &[&str]) -> Self
    {
        let names: Vec<String> = names.iter().map(|&name| name.to_owned()).collect();
        Self::new(image, move |raw| names.iter().any(|name| name == raw))
    }

    /// The file name of the image (its last path component).
    #[must_use]
    pub fn image(&self) -> &str
    {
        &self.image
    }

    /// Whether the raw symbol table name `raw` is an entry to trap.
    #[must_use]
    pub fn is_entry(&self, raw: &str) -> bool
    {
        (self.entry)(raw)
    }
}

impl fmt::Debug for ExitSource
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        f.debug_struct("ExitSource")
            .field("image", &self.image)
            .finish_non_exhaustive()
    }
}

/// A caller frame the unwinder asks [`TargetRuntime::signal_context`] about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrampolineFrame
{
    /// The target's architecture.
    pub architecture: Architecture,
    /// The frame's pc: the return address into it.
    pub pc: Address,
    /// The frame's stack pointer.
    pub sp: Address,
    /// The frame pointer, when this frame's value of it is known.
    pub fp: Option<Address>,
}

/// The register state a signal interrupted, read from where the kernel saved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalContext
{
    /// The general registers in the architecture's order. On arm64, `x0`-`x28`; the frame
    /// pointer and link register are [`fp`](Self::fp) and [`lr`](Self::lr).
    pub general: Vec<u64>,
    /// The interrupted frame pointer.
    pub fp: u64,
    /// The interrupted link register.
    pub lr: u64,
    /// The interrupted stack pointer.
    pub sp: u64,
    /// The interrupted pc, as saved (pointer authentication bits may be set).
    pub pc: u64,
    /// The first address above the saved state (the end of what the kernel pushed). An
    /// interrupted `sp` at or above it was on the same stack; one below the trampoline's
    /// `sp` was on another (an alternate signal stack); any other is rejected.
    pub frame_end: Address,
}
