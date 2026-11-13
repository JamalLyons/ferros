//! The software breakpoint table.
//!
//! The session, not the backend, owns breakpoints. Inserting a software breakpoint
//! reads the original bytes at the address with `Backend::read_memory`, writes the
//! architecture's trap instruction with `Backend::write_memory`, and keeps the original
//! bytes in a [`BreakpointStore`](crate::breakpoints::BreakpointStore) entry. A
//! breakpoint is *inserted* (its trap is in memory) exactly when it is enabled.
//!
//! Every mutation is all or nothing: if a memory access fails, the table is unchanged.

use std::time::SystemTime;

use super::Session;
use crate::arch::{self, Arch};
use crate::backend::BackendError;
use crate::breakpoints::{
    BreakpointEntry, BreakpointId, BreakpointInfo, BreakpointKind, BreakpointPayload, BreakpointRequest, BreakpointState,
};
use crate::error::{DebuggerError, Result};

impl Session
{
    pub(super) fn arch(&self) -> Result<&'static dyn Arch>
    {
        Ok(arch::for_architecture(self.backend.architecture())?)
    }

    /// Add a breakpoint and insert its trap.
    ///
    /// Only [`BreakpointRequest::Software`] is supported. If a software breakpoint
    /// already exists at the address, its id is returned and nothing changes (a disabled
    /// breakpoint stays disabled).
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::Backend`] with
    /// [`BackendError::Unsupported`] for hardware breakpoints, watchpoints, or an
    /// architecture without trap support, [`DebuggerError::InvalidArgument`] if the trap
    /// would overlap another breakpoint's trap, or the backend's error from reading or
    /// writing memory (the table is then unchanged).
    pub fn add_breakpoint(&mut self, request: BreakpointRequest) -> Result<BreakpointId>
    {
        self.require_stopped()?;
        let address = match request {
            BreakpointRequest::Software { address } => address,
            BreakpointRequest::Hardware { .. } => {
                return Err(BackendError::Unsupported("hardware breakpoints are not supported by Session".into()).into());
            }
            BreakpointRequest::Watchpoint { .. } => {
                return Err(BackendError::Unsupported("watchpoints are not supported by Session".into()).into());
            }
        };
        if let Some(existing) = self.breakpoints.id_for_kind(address, BreakpointKind::Software) {
            return Ok(existing);
        }

        let trap = self.arch()?.trap_instruction();
        // A trap that overlaps another breakpoint's trap would save trap bytes as its
        // "original" bytes and corrupt the instruction when both are removed.
        let trap_len = trap.len() as u64;
        if let Some(other) =
            self.breakpoints.list().into_iter().find(|info| {
                info.kind == BreakpointKind::Software && info.address.value().abs_diff(address.value()) < trap_len
            })
        {
            return Err(DebuggerError::InvalidArgument(format!(
                "a breakpoint at 0x{:016x} would overlap the breakpoint at 0x{:016x}",
                address.value(),
                other.address.value()
            )));
        }
        let original = self.backend.read_memory(address, trap.len())?;
        if original.len() != trap.len() {
            return Err(DebuggerError::InvalidArgument(format!(
                "read {} bytes instead of {} at 0x{:016x} to insert a breakpoint",
                original.len(),
                trap.len(),
                address.value()
            )));
        }
        self.backend.write_memory(address, trap)?;

        let mut info = BreakpointInfo::new(BreakpointId::from_raw(0), address, BreakpointKind::Software);
        info.state = BreakpointState::Resolved;
        info.enabled = true;
        info.resolved_at = Some(SystemTime::now());
        Ok(self.breakpoints.insert(BreakpointEntry {
            info,
            payload: BreakpointPayload::Software {
                original_bytes: original,
            },
        }))
    }

    /// Remove a breakpoint, restoring the original bytes if its trap is inserted.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::BreakpointIdNotFound`] for an unknown id, or the
    /// backend's error from writing memory (the breakpoint then stays in the table).
    pub fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        self.require_stopped()?;
        let entry = self.entry(id)?.clone();
        if entry.info.enabled {
            self.restore_original(&entry)?;
        }
        self.breakpoints.remove(id);
        Ok(())
    }

    /// Re-insert a disabled breakpoint's trap. Enabling an enabled breakpoint does nothing.
    ///
    /// The original bytes saved when the breakpoint was added are kept; memory is not
    /// read again.
    ///
    /// # Errors
    ///
    /// As for [`remove_breakpoint`](Self::remove_breakpoint); on a failed write the
    /// breakpoint stays disabled.
    pub fn enable_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        self.require_stopped()?;
        let entry = self.entry(id)?;
        if entry.info.enabled {
            return Ok(());
        }
        let address = entry.info.address;
        let trap = self.arch()?.trap_instruction();
        self.backend.write_memory(address, trap)?;
        if let Some(entry) = self.breakpoints.get_mut(id) {
            entry.info.state = BreakpointState::Resolved;
            entry.info.enabled = true;
            entry.info.resolved_at = Some(SystemTime::now());
        }
        Ok(())
    }

    /// Restore a breakpoint's original bytes and keep it in the table, disabled.
    /// Disabling a disabled breakpoint does nothing.
    ///
    /// # Errors
    ///
    /// As for [`remove_breakpoint`](Self::remove_breakpoint); on a failed write the
    /// breakpoint stays enabled.
    pub fn disable_breakpoint(&mut self, id: BreakpointId) -> Result<()>
    {
        self.require_stopped()?;
        let entry = self.entry(id)?.clone();
        if !entry.info.enabled {
            return Ok(());
        }
        self.restore_original(&entry)?;
        self.mark_disabled(id);
        Ok(())
    }

    /// Public information about one breakpoint.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, or
    /// [`DebuggerError::BreakpointIdNotFound`] for an unknown id.
    pub fn breakpoint_info(&self, id: BreakpointId) -> Result<BreakpointInfo>
    {
        self.require_process()?;
        self.breakpoints.info(id).ok_or(DebuggerError::BreakpointIdNotFound(id.raw()))
    }

    /// Every breakpoint in the table, ordered by id. Empty when there is no process.
    #[must_use]
    pub fn breakpoints(&self) -> Vec<BreakpointInfo>
    {
        let mut list = self.breakpoints.list();
        list.sort_by_key(|info| info.id.raw());
        list
    }

    fn entry(&self, id: BreakpointId) -> Result<&BreakpointEntry>
    {
        self.breakpoints.get(id).ok_or(DebuggerError::BreakpointIdNotFound(id.raw()))
    }

    pub(super) fn mark_disabled(&mut self, id: BreakpointId)
    {
        if let Some(entry) = self.breakpoints.get_mut(id) {
            entry.info.state = BreakpointState::Disabled;
            entry.info.enabled = false;
        }
    }

    /// Write a software breakpoint's saved original bytes back to memory.
    pub(super) fn restore_original(&mut self, entry: &BreakpointEntry) -> Result<()>
    {
        if let BreakpointPayload::Software { original_bytes } = &entry.payload {
            self.backend.write_memory(entry.info.address, original_bytes)?;
        }
        Ok(())
    }

    /// Restore every inserted trap, marking each restored breakpoint disabled.
    ///
    /// Stops at the first failed write and returns its error.
    pub(super) fn restore_all_traps(&mut self) -> Result<()>
    {
        let mut inserted: Vec<BreakpointEntry> = self
            .breakpoints
            .list()
            .into_iter()
            .filter(|info| info.enabled)
            .filter_map(|info| self.breakpoints.get(info.id).cloned())
            .collect();
        inserted.sort_by_key(|entry| entry.info.id.raw());
        for entry in inserted {
            self.restore_original(&entry)?;
            self.mark_disabled(entry.info.id);
        }
        Ok(())
    }
}

// Compute overlaps in u128 so a range near the address-space end cannot wrap.
fn overlap(
    address: crate::types::Address,
    len: usize,
    base: crate::types::Address,
    size: usize,
) -> Option<(usize, usize, usize)>
{
    let a = u128::from(address.value());
    let b = u128::from(base.value());
    let start = a.max(b);
    let end = (a + len as u128).min(b + size as u128);
    (start < end).then(|| ((start - a) as usize, (start - b) as usize, (end - start) as usize))
}

impl Session
{
    pub(super) fn overlay_originals(&self, address: crate::types::Address, data: &mut [u8])
    {
        for info in self.breakpoints.list().into_iter().filter(|info| info.enabled) {
            if let Some(BreakpointEntry {
                payload: BreakpointPayload::Software { original_bytes },
                ..
            }) = self.breakpoints.get(info.id)
                && let Some((offset, original, len)) = overlap(address, data.len(), info.address, original_bytes.len())
            {
                data[offset..offset + len].copy_from_slice(&original_bytes[original..original + len]);
            }
        }
    }

    pub(super) fn write_through_breakpoints(&mut self, address: crate::types::Address, data: &[u8]) -> Result<()>
    {
        let mut written = data.to_vec();
        let mut updates = Vec::new();
        for info in self.breakpoints.list() {
            let Some(mut entry) = self.breakpoints.get(info.id).cloned() else {
                continue;
            };
            if let BreakpointPayload::Software { original_bytes } = &mut entry.payload
                && let Some((offset, original, len)) = overlap(address, data.len(), info.address, original_bytes.len())
            {
                original_bytes[original..original + len].copy_from_slice(&data[offset..offset + len]);
                if info.enabled {
                    let trap = self.arch()?.trap_instruction();
                    written[offset..offset + len].copy_from_slice(&trap[original..original + len]);
                }
                updates.push(entry);
            }
        }
        self.backend.write_memory(address, &written)?;
        for entry in updates {
            if let Some(stored) = self.breakpoints.get_mut(entry.info.id) {
                *stored = entry;
            }
        }
        Ok(())
    }
}
