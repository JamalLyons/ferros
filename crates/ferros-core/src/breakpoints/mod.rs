//! Breakpoint and watchpoint bookkeeping.
//!
//! This module centralizes breakpoint lifecycle tracking so debugger backends can
//! focus on the platform-specific mechanics (patching memory, configuring debug
//! registers, etc.). The backend is responsible for actually installing /
//! restoring traps but can use this structure to track ids, states, and hit
//! counts in a thread-safe manner.

pub mod builder;
use std::collections::HashMap;
use std::time::SystemTime;

use crate::types::Address;

/// Unique identifier for a breakpoint managed by the debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BreakpointId(u64);

impl BreakpointId
{
    /// Create a new identifier from a raw value.
    #[must_use]
    pub const fn from_raw(value: u64) -> Self
    {
        Self(value)
    }

    /// Get the raw numeric representation (useful for logging / errors).
    #[must_use]
    pub const fn raw(self) -> u64
    {
        self.0
    }
}

/// Different kinds of breakpoints / watchpoints that can be tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakpointKind
{
    /// Software breakpoint implemented via trap instructions (BRK/INT3).
    Software,
    /// Hardware breakpoint configured via CPU debug registers.
    Hardware,
    /// Data watchpoint (triggers on memory access).
    Watchpoint,
}

/// Access type for data watchpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchpointAccess
{
    /// Trigger on read access to the watched memory region.
    Read,
    /// Trigger on write access to the watched memory region.
    Write,
    /// Trigger on either read or write access to the watched memory region.
    ReadWrite,
}

/// High-level request to create a breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakpointRequest
{
    /// Software breakpoint at an address.
    Software
    {
        /// The memory address where the breakpoint should be placed.
        address: Address
    },
    /// Hardware execution breakpoint.
    Hardware
    {
        /// The memory address where the breakpoint should be placed.
        address: Address
    },
    /// Watchpoint on a memory range.
    Watchpoint
    {
        /// The starting memory address of the watched region.
        address: Address,
        /// The length in bytes of the memory region to watch.
        length: usize,
        /// The type of memory access that should trigger the watchpoint.
        access: WatchpointAccess,
    },
}

/// Lifecycle states for a breakpoint entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakpointState
{
    /// Requested but not yet installed.
    Requested,
    /// Installed and will trigger when hit.
    Resolved,
    /// Temporarily disabled (trap removed).
    Disabled,
}

/// Public information about a breakpoint.
#[derive(Debug, Clone)]
pub struct BreakpointInfo
{
    /// Unique identifier for this breakpoint.
    pub id: BreakpointId,
    /// The memory address where the breakpoint is placed.
    pub address: Address,
    /// The type of breakpoint (software, hardware, or watchpoint).
    pub kind: BreakpointKind,
    /// Current lifecycle state of the breakpoint.
    pub state: BreakpointState,
    /// Whether the breakpoint is currently enabled and will trigger.
    pub enabled: bool,
    /// Number of times this breakpoint has been hit.
    pub hit_count: u64,
    /// Timestamp when the breakpoint was first requested.
    pub requested_at: SystemTime,
    /// Timestamp when the breakpoint was successfully installed, if resolved.
    pub resolved_at: Option<SystemTime>,
    /// Access type for watchpoints (None for execution breakpoints).
    pub watch_access: Option<WatchpointAccess>,
    /// Length in bytes for watchpoints (None for execution breakpoints).
    pub watch_length: Option<usize>,
}

impl BreakpointInfo
{
    /// Create a new breakpoint info with the given identifier, address, and kind.
    /// The breakpoint starts in `Requested` state, disabled, with zero hit count.
    #[must_use]
    pub fn new(id: BreakpointId, address: Address, kind: BreakpointKind) -> Self
    {
        Self {
            id,
            address,
            kind,
            state: BreakpointState::Requested,
            enabled: false,
            hit_count: 0,
            requested_at: SystemTime::now(),
            resolved_at: None,
            watch_access: None,
            watch_length: None,
        }
    }
}

/// Internal payload used by the backend to restore state.
#[derive(Debug, Clone)]
pub enum BreakpointPayload
{
    /// Payload for software breakpoints.
    Software
    {
        /// Original instruction bytes that were replaced by the trap instruction.
        original_bytes: Vec<u8>
    },
    /// Payload for hardware breakpoints.
    Hardware
    {
        /// The memory address configured in the debug register.
        address: Address,
        /// The debug register slot number used for this breakpoint.
        slot: u32
    },
    /// Payload for watchpoints.
    Watchpoint
    {
        /// Length in bytes of the watched memory region.
        length: usize,
        /// Type of access that triggers this watchpoint.
        access: WatchpointAccess
    },
}

/// Breakpoint entry tracked by the manager.
#[derive(Debug, Clone)]
pub struct BreakpointEntry
{
    /// Public information about the breakpoint.
    pub info: BreakpointInfo,
    /// Internal payload used by the backend to restore state.
    pub payload: BreakpointPayload,
}

/// Serializable snapshot used when draining the store.
pub type BreakpointRecord = BreakpointEntry;

/// Thread-safe breakpoint store (wrap in Mutex/Arc in backends).
#[derive(Debug, Default)]
pub struct BreakpointStore
{
    next_id: u64,
    by_id: HashMap<BreakpointId, BreakpointEntry>,
    by_kind: HashMap<(Address, BreakpointKind), BreakpointId>,
}

impl BreakpointStore
{
    /// Create a new empty breakpoint store.
    #[must_use]
    pub fn new() -> Self
    {
        Self::default()
    }

    fn allocate_id(&mut self) -> BreakpointId
    {
        self.next_id = self.next_id.wrapping_add(1);
        BreakpointId::from_raw(self.next_id)
    }

    /// Insert a new breakpoint entry and return the identifier that should be used
    /// by the backend for future lookups. If the provided entry already contains an
    /// id (non-zero), it is preserved; otherwise a fresh id is allocated.
    pub fn insert(&mut self, mut entry: BreakpointEntry) -> BreakpointId
    {
        let id = if entry.info.id.raw() == 0 {
            let allocated = self.allocate_id();
            entry.info.id = allocated;
            allocated
        } else {
            entry.info.id
        };
        self.by_kind.insert((entry.info.address, entry.info.kind), id);
        self.by_id.insert(id, entry);
        id
    }

    /// Retrieve an immutable reference to a breakpoint entry by id.
    pub fn get(&self, id: BreakpointId) -> Option<&BreakpointEntry>
    {
        self.by_id.get(&id)
    }

    /// Retrieve a mutable reference to a breakpoint entry by id.
    pub fn get_mut(&mut self, id: BreakpointId) -> Option<&mut BreakpointEntry>
    {
        self.by_id.get_mut(&id)
    }

    /// Return the identifier that matches a given address and kind combination, if
    /// one exists.
    pub fn id_for_kind(&self, address: Address, kind: BreakpointKind) -> Option<BreakpointId>
    {
        self.by_kind.get(&(address, kind)).copied()
    }

    /// Remove a breakpoint from the store, returning the entry if it was present.
    pub fn remove(&mut self, id: BreakpointId) -> Option<BreakpointEntry>
    {
        if let Some(entry) = self.by_id.remove(&id) {
            self.by_kind.remove(&(entry.info.address, entry.info.kind));
            Some(entry)
        } else {
            None
        }
    }

    /// List all breakpoint infos currently tracked by the store. Each info value
    /// represents the public state for the corresponding entry.
    pub fn list(&self) -> Vec<BreakpointInfo>
    {
        self.by_id.values().map(|entry| entry.info.clone()).collect()
    }

    /// Fetch the public info for a specific breakpoint id.
    pub fn info(&self, id: BreakpointId) -> Option<BreakpointInfo>
    {
        self.by_id.get(&id).map(|entry| entry.info.clone())
    }

    /// Record that a breakpoint at the provided address was hit. The entry's hit
    /// counter is incremented only if the breakpoint is currently enabled.
    pub fn record_hit(&mut self, address: Address) -> Option<BreakpointInfo>
    {
        let id = self
            .id_for_kind(address, BreakpointKind::Software)
            .or_else(|| self.id_for_kind(address, BreakpointKind::Hardware))?;
        let entry = self.by_id.get_mut(&id)?;
        if !entry.info.enabled {
            return None;
        }
        entry.info.hit_count = entry.info.hit_count.saturating_add(1);
        Some(entry.info.clone())
    }

    /// Drain the store, returning all entries and resetting the internal
    /// bookkeeping maps.
    pub fn drain(&mut self) -> Vec<BreakpointRecord>
    {
        self.by_kind.clear();
        self.by_id.drain().map(|(_, entry)| entry).collect()
    }
}
