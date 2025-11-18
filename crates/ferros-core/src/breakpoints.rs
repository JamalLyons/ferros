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
    Read,
    Write,
    ReadWrite,
}

/// High-level request to create a breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakpointRequest
{
    /// Software breakpoint at an address.
    Software
    {
        address: Address
    },
    /// Hardware execution breakpoint.
    Hardware
    {
        address: Address
    },
    /// Watchpoint on a memory range.
    Watchpoint
    {
        address: Address,
        length: usize,
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
    pub id: BreakpointId,
    pub address: Address,
    pub kind: BreakpointKind,
    pub state: BreakpointState,
    pub enabled: bool,
    pub hit_count: u64,
    pub requested_at: SystemTime,
    pub resolved_at: Option<SystemTime>,
    pub watch_access: Option<WatchpointAccess>,
    pub watch_length: Option<usize>,
}

impl BreakpointInfo
{
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
    Software
    {
        original_bytes: Vec<u8>
    },
    Hardware
    {
        address: Address, slot: u32
    },
    Watchpoint
    {
        length: usize, access: WatchpointAccess
    },
}

/// Breakpoint entry tracked by the manager.
#[derive(Debug, Clone)]
pub struct BreakpointEntry
{
    pub info: BreakpointInfo,
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

    pub fn get(&self, id: BreakpointId) -> Option<&BreakpointEntry>
    {
        self.by_id.get(&id)
    }

    pub fn get_mut(&mut self, id: BreakpointId) -> Option<&mut BreakpointEntry>
    {
        self.by_id.get_mut(&id)
    }

    pub fn id_for_kind(&self, address: Address, kind: BreakpointKind) -> Option<BreakpointId>
    {
        self.by_kind.get(&(address, kind)).copied()
    }

    pub fn remove(&mut self, id: BreakpointId) -> Option<BreakpointEntry>
    {
        if let Some(entry) = self.by_id.remove(&id) {
            self.by_kind.remove(&(entry.info.address, entry.info.kind));
            Some(entry)
        } else {
            None
        }
    }

    pub fn list(&self) -> Vec<BreakpointInfo>
    {
        self.by_id.values().map(|entry| entry.info.clone()).collect()
    }

    pub fn info(&self, id: BreakpointId) -> Option<BreakpointInfo>
    {
        self.by_id.get(&id).map(|entry| entry.info.clone())
    }

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

    pub fn drain(&mut self) -> Vec<BreakpointRecord>
    {
        self.by_kind.clear();
        self.by_id.drain().map(|(_, entry)| entry).collect()
    }
}
