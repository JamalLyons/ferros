//! The software breakpoint table.
//!
//! The session, not the backend, owns breakpoints. Inserting a software breakpoint
//! reads the original bytes at the address with `Backend::read_memory`, writes the
//! architecture's trap instruction with `Backend::write_memory`, and keeps the original
//! bytes in a [`BreakpointStore`](crate::breakpoints::BreakpointStore) entry. A
//! breakpoint is *inserted* (its trap is in memory) exactly when it is enabled.
//!
//! Logical delete/disable rolls back earlier trap writes if a later write fails. If
//! rollback itself fails, or a step-over cannot re-arm a trap, the unarmed entry and
//! every logical owner are marked disabled. Other sites can remain armed; enabling
//! the logical breakpoint again repairs them, and deleting it releases them.

//!
//! # Logical breakpoints
//!
//! On top of that address table, [`Session::set_breakpoint`] adds a *logical* breakpoint
//! for a [`BreakpointLocation`] (a source line, a function name or an address). One
//! logical breakpoint can resolve to several addresses (*sites*: inlined copies, generic
//! instantiations), and is listed, counted, disabled and deleted as one. Each site is a
//! software breakpoint in the address table; several logical breakpoints at one address
//! share its trap, which stays inserted while any of them is enabled.
//!
//! The address table does not record which logical breakpoint created an entry, so a
//! caller should not mix [`Session::add_breakpoint`] / [`Session::remove_breakpoint`]
//! with logical breakpoints at the same address: deleting the logical breakpoint removes
//! the trap, and removing the trap leaves the logical breakpoint without it.
//!
//! A logical breakpoint set while there is no process is *pending*. Every source
//! breakpoint is resolved again when a process is launched or attached, at its first stop,
//! so it holds across runs; a failure leaves it pending with the reason. Address
//! breakpoints mean nothing in another process and are deleted when the process ends.

use std::time::SystemTime;

use super::Session;
use super::location::{BreakpointLocation, BreakpointSite};
use crate::arch::{self, Arch};
use crate::backend::BackendError;
use crate::breakpoints::{
    BreakpointEntry, BreakpointId, BreakpointInfo, BreakpointKind, BreakpointPayload, BreakpointRequest, BreakpointState,
};
use crate::error::{DebuggerError, Result};
use crate::types::Address;

/// Identifier of a logical breakpoint (see [`Session::set_breakpoint`]). Numbered from 1,
/// separately from the ids of the address table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalBreakpointId(u64);

impl LogicalBreakpointId
{
    /// An identifier from its number.
    #[must_use]
    pub const fn from_raw(value: u64) -> Self
    {
        Self(value)
    }

    /// The number.
    #[must_use]
    pub const fn raw(self) -> u64
    {
        self.0
    }
}

/// A logical breakpoint: one requested location and every address it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalBreakpoint
{
    /// Its identifier.
    pub id: LogicalBreakpointId,
    /// What was asked for.
    pub location: BreakpointLocation,
    /// Whether all its sites are armed (or requested while pending). Cleared if any
    /// site cannot be re-armed; other sites may still trap until deleted or re-enabled.
    pub enabled: bool,
    /// Stops at any of its sites while it was enabled, over every run.
    pub hit_count: u64,
    /// The addresses it resolved to in the current process, sorted. Empty while pending.
    pub sites: Vec<BreakpointSite>,
    /// Why it has no sites: `None` once resolved in the current process, otherwise the
    /// reason (no process yet, or the lookup error of the last attempt).
    pub pending: Option<String>,
}

impl LogicalBreakpoint
{
    /// Its site at `address`, if it has one.
    #[must_use]
    pub fn site_at(&self, address: Address) -> Option<&BreakpointSite>
    {
        self.sites.iter().find(|site| site.address == address)
    }
}

/// A stop at a logical breakpoint, as returned by [`Session::breakpoint_hit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointHit
{
    /// The logical breakpoint that stopped the program.
    pub id: LogicalBreakpointId,
    /// The thread that hit it.
    pub thread: crate::types::ThreadId,
    /// The site it stopped at: its address, function and source line, exactly as the
    /// breakpoint reported them when it was resolved.
    pub site: BreakpointSite,
}

/// Pending reason of a breakpoint set while there is no process.
const NO_PROCESS: &str = "the program is not running";

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
            let address = entry.info.address;
            for breakpoint in &mut self.logical {
                if breakpoint.site_at(address).is_some() {
                    breakpoint.enabled = false;
                }
            }
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
    /// # Errors
    ///
    /// Stops at the first failed write and returns its error. Already restored entries
    /// and all their logical owners are disabled; remaining traps stay inserted.
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

/// The saved original bytes of every enabled software breakpoint, by trap address, so
/// memory read past the session (the stack unwinder's) can hide traps too.
pub(super) type TrapOriginals = Vec<(crate::types::Address, Vec<u8>)>;

/// Replace the trap bytes in `data`, read at `address`, with their saved originals.
pub(super) fn overlay_traps(traps: &TrapOriginals, address: crate::types::Address, data: &mut [u8])
{
    for (base, original_bytes) in traps {
        if let Some((offset, original, len)) = overlap(address, data.len(), *base, original_bytes.len()) {
            data[offset..offset + len].copy_from_slice(&original_bytes[original..original + len]);
        }
    }
}

impl Session
{
    pub(super) fn trap_originals(&self) -> TrapOriginals
    {
        self.breakpoints
            .list()
            .into_iter()
            .filter(|info| info.enabled)
            .filter_map(|info| match self.breakpoints.get(info.id) {
                Some(BreakpointEntry {
                    payload: BreakpointPayload::Software { original_bytes },
                    ..
                }) => Some((info.address, original_bytes.clone())),
                _ => None,
            })
            .collect()
    }

    pub(super) fn overlay_originals(&self, address: crate::types::Address, data: &mut [u8])
    {
        overlay_traps(&self.trap_originals(), address, data);
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

impl Session
{
    /// Add a logical breakpoint at `location` and insert a trap at each of its sites.
    ///
    /// With a stopped process the location is resolved now (see the
    /// [`location`](super::location) module) and an error adds nothing. With no process,
    /// a source location is kept pending and resolved when one is launched or attached.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotStopped`] while running; [`DebuggerError::NotAttached`] for an
    /// address with no process; [`DebuggerError::SourceLookup`] for an unknown or
    /// ambiguous file or function, or a line with no code at or after it; the main image's
    /// symbol-loading error; or an error inserting a trap (as for
    /// [`add_breakpoint`](Self::add_breakpoint); traps already inserted are removed).
    pub fn set_breakpoint(&mut self, location: BreakpointLocation) -> Result<LogicalBreakpointId>
    {
        let (sites, pending) = match self.state {
            super::SessionState::Running => return Err(DebuggerError::NotStopped),
            super::SessionState::Stopped => {
                let sites = self.resolve_location(&location)?;
                self.insert_sites(&sites)?;
                (sites, None)
            }
            super::SessionState::NotStarted | super::SessionState::Exited => {
                if matches!(location, BreakpointLocation::Address(_)) {
                    return Err(DebuggerError::NotAttached);
                }
                (Vec::new(), Some(NO_PROCESS.to_owned()))
            }
        };
        self.next_logical = self.next_logical.wrapping_add(1);
        let id = LogicalBreakpointId(self.next_logical);
        self.logical.push(LogicalBreakpoint {
            id,
            location,
            enabled: true,
            hit_count: 0,
            sites,
            pending,
        });
        Ok(id)
    }

    /// Delete a logical breakpoint, removing the traps no other logical breakpoint uses.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotStopped`] while running, [`DebuggerError::BreakpointIdNotFound`]
    /// for an unknown id, or an error restoring a trap. The breakpoint is kept and
    /// earlier writes are rolled back, preserving its sites and table entries. If a
    /// rollback write also fails, that site and every logical owner are marked disabled
    /// (other sites may remain armed); the original restoration error is returned.
    pub fn delete_breakpoint(&mut self, id: LogicalBreakpointId) -> Result<()>
    {
        self.require_not_running()?;
        let index = self.logical_index(id)?;
        // Also for a disabled breakpoint: a trap entry it shared with breakpoints since
        // deleted must not outlive the last of them.
        let addresses = self.site_addresses(index);
        self.release_sites(id, &addresses)?;
        self.logical.remove(index);
        Ok(())
    }

    /// Enable a logical breakpoint: insert the traps of its sites, resolving it first if
    /// it is pending and there is a process. Enabling an enabled breakpoint does nothing.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotStopped`] while running, [`DebuggerError::BreakpointIdNotFound`]
    /// for an unknown id, the lookup error of a pending breakpoint, or an error inserting
    /// a trap; the breakpoint then stays disabled.
    pub fn enable_logical_breakpoint(&mut self, id: LogicalBreakpointId) -> Result<()>
    {
        self.require_not_running()?;
        let index = self.logical_index(id)?;
        if self.logical[index].enabled {
            return Ok(());
        }
        if self.state == super::SessionState::Stopped {
            let sites = if self.logical[index].pending.is_some() {
                let location = self.logical[index].location.clone();
                self.resolve_location(&location)?
            } else {
                self.logical[index].sites.clone()
            };
            self.insert_sites(&sites)?;
            let breakpoint = &mut self.logical[index];
            breakpoint.sites = sites;
            breakpoint.pending = None;
        }
        self.logical[index].enabled = true;
        Ok(())
    }

    /// Disable a logical breakpoint: restore the traps no other enabled logical breakpoint
    /// uses, keeping its sites. Disabling a disabled breakpoint does nothing.
    ///
    /// # Errors
    ///
    /// As for [`delete_breakpoint`](Self::delete_breakpoint): a failed write rolls back
    /// earlier sites and keeps the breakpoint enabled, unless rollback also fails, in
    /// which case logical owners of any unarmed site are marked disabled.
    pub fn disable_logical_breakpoint(&mut self, id: LogicalBreakpointId) -> Result<()>
    {
        self.require_not_running()?;
        let index = self.logical_index(id)?;
        if !self.logical[index].enabled {
            return Ok(());
        }
        let addresses = self.site_addresses(index);
        self.release_sites(id, &addresses)?;
        self.logical[index].enabled = false;
        Ok(())
    }

    /// Every logical breakpoint, ordered by id.
    #[must_use]
    pub fn logical_breakpoints(&self) -> &[LogicalBreakpoint]
    {
        &self.logical
    }

    /// The enabled logical breakpoint with the lowest id that has a site at `address`:
    /// the one a breakpoint stop at `address` belongs to.
    #[must_use]
    pub fn logical_breakpoint_at(&self, address: Address) -> Option<&LogicalBreakpoint>
    {
        self.logical
            .iter()
            .find(|breakpoint| breakpoint.enabled && breakpoint.sites.iter().any(|site| site.address == address))
    }

    /// The logical breakpoint the last stop belongs to, when it was a breakpoint stop at
    /// one of its sites.
    #[must_use]
    pub fn breakpoint_hit(&self) -> Option<BreakpointHit>
    {
        let crate::backend::StopEvent::Breakpoint { thread, pc } = self.last_stop.as_ref()? else {
            return None;
        };
        let breakpoint = self.logical_breakpoint_at(*pc)?;
        Some(BreakpointHit {
            id: breakpoint.id,
            thread: *thread,
            site: breakpoint.site_at(*pc)?.clone(),
        })
    }

    /// Count a breakpoint stop at `address` for every enabled logical breakpoint there.
    pub(super) fn record_logical_hit(&mut self, address: Address)
    {
        for breakpoint in &mut self.logical {
            if breakpoint.enabled && breakpoint.sites.iter().any(|site| site.address == address) {
                breakpoint.hit_count = breakpoint.hit_count.saturating_add(1);
            }
        }
    }

    /// The process ended: address breakpoints are deleted, the others become pending.
    pub(super) fn forget_logical_sites(&mut self)
    {
        self.logical
            .retain(|breakpoint| !matches!(breakpoint.location, BreakpointLocation::Address(_)));
        for breakpoint in &mut self.logical {
            breakpoint.sites.clear();
            breakpoint.pending = Some(NO_PROCESS.to_owned());
        }
    }

    /// Resolve every logical breakpoint in a new process at its first stop. A failure
    /// leaves that breakpoint pending with the reason; it never fails the launch.
    pub(super) fn resolve_logical_breakpoints(&mut self)
    {
        for index in 0..self.logical.len() {
            let location = self.logical[index].location.clone();
            let resolved = self.resolve_location(&location).and_then(|sites| {
                if self.logical[index].enabled {
                    self.insert_sites(&sites)?;
                }
                Ok(sites)
            });
            let breakpoint = &mut self.logical[index];
            match resolved {
                Ok(sites) => {
                    breakpoint.sites = sites;
                    breakpoint.pending = None;
                }
                Err(error) => {
                    tracing::debug!("breakpoint {} at {} stays pending: {error}", breakpoint.id.raw(), location);
                    breakpoint.sites.clear();
                    breakpoint.pending = Some(error.to_string());
                }
            }
        }
    }

    fn require_not_running(&self) -> Result<()>
    {
        if self.state == super::SessionState::Running {
            Err(DebuggerError::NotStopped)
        } else {
            Ok(())
        }
    }

    fn logical_index(&self, id: LogicalBreakpointId) -> Result<usize>
    {
        self.logical
            .iter()
            .position(|breakpoint| breakpoint.id == id)
            .ok_or(DebuggerError::BreakpointIdNotFound(id.raw()))
    }

    fn site_addresses(&self, index: usize) -> Vec<Address>
    {
        self.logical[index].sites.iter().map(|site| site.address).collect()
    }

    /// Insert (or re-enable) a trap at every site. All or nothing: on an error, the traps
    /// this call inserted or enabled are taken out again.
    fn insert_sites(&mut self, sites: &[BreakpointSite]) -> Result<()>
    {
        let mut added = Vec::new();
        let mut enabled = Vec::new();
        let mut result = Ok(());
        for site in sites {
            match self.breakpoints.id_for_kind(site.address, BreakpointKind::Software) {
                Some(id) if self.breakpoints.info(id).is_some_and(|info| info.enabled) => {}
                Some(id) => match self.enable_breakpoint(id) {
                    Ok(()) => enabled.push(id),
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                },
                None => match self.add_breakpoint(BreakpointRequest::Software { address: site.address }) {
                    Ok(id) => added.push(id),
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                },
            }
        }
        if result.is_err() {
            for id in added {
                let _ = self.remove_breakpoint(id);
            }
            for id in enabled {
                let _ = self.disable_breakpoint(id);
            }
        }
        result
    }

    /// Take out the traps at `addresses` that no enabled logical breakpoint other than
    /// `owner` uses: removed when no other logical breakpoint has a site there, otherwise
    /// disabled. Write originals before changing the table so failure can roll back
    /// without losing ids, metadata, or the original bytes.
    fn release_sites(&mut self, owner: LogicalBreakpointId, addresses: &[Address]) -> Result<()>
    {
        let mut released: Vec<(BreakpointEntry, bool)> = Vec::new();
        for &address in addresses {
            let others = self
                .logical
                .iter()
                .filter(|breakpoint| breakpoint.id != owner && breakpoint.sites.iter().any(|site| site.address == address));
            let (mut shared, mut enabled) = (false, false);
            for other in others {
                shared = true;
                enabled |= other.enabled;
            }
            if enabled {
                continue;
            }
            let Some(id) = self.breakpoints.id_for_kind(address, BreakpointKind::Software) else {
                continue;
            };
            let entry = self.entry(id)?.clone();
            if entry.info.enabled
                && let Err(error) = self.restore_original(&entry)
            {
                for (previous, _) in released.iter().rev().filter(|(entry, _)| entry.info.enabled) {
                    let rollback = self
                        .arch()
                        .and_then(|arch| Ok(self.backend.write_memory(previous.info.address, arch.trap_instruction())?));
                    if let Err(rollback_error) = rollback {
                        self.mark_disabled(previous.info.id);
                        tracing::warn!("could not roll back breakpoint {}: {rollback_error}", previous.info.id.raw());
                    }
                }
                return Err(error);
            }
            released.push((entry, shared));
        }
        for (entry, shared) in released {
            if shared {
                self.mark_disabled(entry.info.id);
            } else {
                self.breakpoints.remove(entry.info.id);
            }
        }
        Ok(())
    }
}
