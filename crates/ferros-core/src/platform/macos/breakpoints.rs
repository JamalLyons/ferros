//! # macOS Breakpoint Management
//!
//! Breakpoint installation and management for macOS debugger.
//!
//! This module handles both software breakpoints (INT3/BRK instructions) and
//! hardware breakpoints (CPU debug registers). It provides methods to install,
//! remove, enable, and disable breakpoints.
//!
//! ## Breakpoint Types
//!
//! - **Software breakpoints**: Modify code by replacing instructions with trap
//!   instructions (INT3 on x86-64, BRK on ARM64). Limited only by available memory.
//! - **Hardware breakpoints**: Use CPU debug registers (DR0-DR7 on x86-64,
//!   DBGBVR/DBGBCR on ARM64). Limited to 4 on x86-64, 16 on ARM64.
//!
//! ## References
//!
//! - [registers::debug](super::registers::debug) for hardware breakpoint implementation
//! - [BreakpointStore](../breakpoints/struct.BreakpointStore.html) for breakpoint bookkeeping

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use libc::thread_act_t;

use crate::breakpoints::{
    BreakpointEntry, BreakpointId, BreakpointInfo, BreakpointKind, BreakpointPayload, BreakpointRequest, BreakpointState,
    BreakpointStore,
};
use crate::error::{DebuggerError, Result};
use crate::platform::macos::{constants, registers};
use crate::types::{Address, Architecture};

/// Trait for breakpoint operations that require access to debugger internals.
///
/// This trait allows the breakpoint module to access memory read/write operations
/// and thread lists without exposing the entire `MacOSDebugger` struct.
pub(crate) trait BreakpointOperations
{
    /// Read memory from the target process.
    fn read_memory(&self, addr: Address, len: usize) -> Result<Vec<u8>>;

    /// Write memory to the target process.
    fn write_memory(&mut self, addr: Address, data: &[u8]) -> Result<usize>;

    /// Get the list of thread ports.
    fn thread_ports(&self) -> &[thread_act_t];

    /// Get the target architecture.
    fn architecture(&self) -> Architecture;

    /// Ensure the debugger is attached to a process.
    fn ensure_attached(&self) -> Result<()>;
}

/// Breakpoint management functions for macOS debugger.
pub(crate) struct BreakpointManager;

impl BreakpointManager
{
    /// Get the trap instruction bytes for the current architecture.
    ///
    /// Returns the instruction sequence that triggers a breakpoint:
    /// - ARM64: `BRK #0` (0x00, 0x00, 0x20, 0xD4)
    /// - x86-64: `INT3` (0xCC)
    pub(crate) fn software_trap_bytes(architecture: Architecture) -> Result<Vec<u8>>
    {
        match architecture {
            Architecture::Arm64 => {
                #[cfg(target_arch = "aarch64")]
                {
                    Ok(constants::ARM64_BRK_INSTRUCTION.to_vec())
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    Err(DebuggerError::InvalidArgument(
                        "ARM64 breakpoints not supported on this build".to_string(),
                    ))
                }
            }
            Architecture::X86_64 => {
                #[cfg(target_arch = "x86_64")]
                {
                    Ok(constants::X86_64_INT3_INSTRUCTION.to_vec())
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    Err(DebuggerError::InvalidArgument(
                        "x86-64 breakpoints not supported on this build".to_string(),
                    ))
                }
            }
            Architecture::Unknown(label) => Err(DebuggerError::InvalidArgument(format!(
                "Software breakpoints unsupported for architecture: {label}"
            ))),
        }
    }

    /// Install a software breakpoint at the given address.
    ///
    /// This replaces the instruction at `address` with a trap instruction
    /// (INT3 on x86-64, BRK on ARM64). The original instruction is saved
    /// so it can be restored later.
    pub(crate) fn install_software_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        address: Address,
    ) -> Result<BreakpointId>
    {
        ops.ensure_attached()?;
        let trap = BreakpointManager::software_trap_bytes(ops.architecture())?;

        {
            let store = breakpoints.lock().unwrap();
            if store.id_for_kind(address, BreakpointKind::Software).is_some() {
                return Err(DebuggerError::InvalidArgument(format!(
                    "Breakpoint already exists at 0x{:016x}",
                    address.value()
                )));
            }
        }

        let original = ops.read_memory(address, trap.len())?;
        if original.len() != trap.len() {
            return Err(DebuggerError::InvalidArgument(format!(
                "Unable to read {} bytes at 0x{:016x} to install breakpoint",
                trap.len(),
                address.value()
            )));
        }

        let written = ops.write_memory(address, &trap)?;
        if written != trap.len() {
            return Err(DebuggerError::InvalidArgument(format!(
                "Failed to write breakpoint trap at 0x{:016x}",
                address.value()
            )));
        }

        let mut info = BreakpointInfo::new(BreakpointId::from_raw(0), address, BreakpointKind::Software);
        info.state = BreakpointState::Resolved;
        info.enabled = true;
        info.resolved_at = Some(SystemTime::now());

        let entry = BreakpointEntry {
            info,
            payload: BreakpointPayload::Software {
                original_bytes: original,
            },
        };

        let mut store = breakpoints.lock().unwrap();
        Ok(store.insert(entry))
    }

    /// Install a hardware breakpoint at the given address.
    ///
    /// This uses CPU debug registers to break on instruction execution.
    /// Hardware breakpoints are installed on all threads in the process.
    pub(crate) fn install_hardware_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        address: Address,
    ) -> Result<BreakpointId>
    {
        ops.ensure_attached()?;

        {
            let store = breakpoints.lock().unwrap();
            if store.id_for_kind(address, BreakpointKind::Hardware).is_some() {
                return Err(DebuggerError::InvalidArgument(format!(
                    "Hardware breakpoint already exists at 0x{:016x}",
                    address.value()
                )));
            }
        }

        // Install on all threads
        let mut used_slot = None;

        for &thread in ops.thread_ports() {
            let slot = registers::set_hardware_breakpoint(thread, address)?;
            if let Some(s) = used_slot {
                if s != slot {
                    tracing::warn!("Hardware breakpoint slots inconsistent across threads: {} vs {}", s, slot);
                }
            } else {
                used_slot = Some(slot);
            }
        }

        let slot = used_slot
            .ok_or_else(|| DebuggerError::AttachFailed("No threads available to set hardware breakpoint".into()))?;

        let mut info = BreakpointInfo::new(BreakpointId::from_raw(0), address, BreakpointKind::Hardware);
        info.state = BreakpointState::Resolved;
        info.enabled = true;
        info.resolved_at = Some(SystemTime::now());

        let entry = BreakpointEntry {
            info,
            payload: BreakpointPayload::Hardware { address, slot },
        };

        let mut store = breakpoints.lock().unwrap();
        Ok(store.insert(entry))
    }

    /// Restore a software breakpoint by writing back the original instruction.
    pub(crate) fn restore_software_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        entry: &BreakpointEntry,
    ) -> Result<()>
    {
        if let BreakpointPayload::Software { original_bytes } = &entry.payload {
            let written = ops.write_memory(entry.info.address, original_bytes)?;
            if written != original_bytes.len() {
                return Err(DebuggerError::InvalidArgument(format!(
                    "Failed to restore original instruction at 0x{:016x}",
                    entry.info.address.value()
                )));
            }
        }
        Ok(())
    }

    /// Remove a hardware breakpoint from all threads.
    pub(crate) fn remove_hardware_breakpoint<Ops: BreakpointOperations>(ops: &Ops, entry: &BreakpointEntry) -> Result<()>
    {
        if let BreakpointPayload::Hardware { slot, .. } = &entry.payload {
            for &thread in ops.thread_ports() {
                // Best effort removal
                if let Err(e) = registers::clear_hardware_breakpoint(thread, *slot) {
                    tracing::warn!("Failed to clear hardware breakpoint on thread {}: {}", thread, e);
                }
            }
        }
        Ok(())
    }

    /// Restore all breakpoints (remove software breakpoints, clear hardware breakpoints).
    pub(crate) fn restore_all_breakpoints<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
    )
    {
        let entries = {
            let mut store = breakpoints.lock().unwrap();
            store.drain()
        };

        for entry in entries {
            if let BreakpointPayload::Software { .. } = entry.payload {
                if let Err(err) = Self::restore_software_breakpoint(ops, &entry) {
                    tracing::warn!("Failed to restore breakpoint 0x{:016x}: {err}", entry.info.address.value());
                }
            } else if let BreakpointPayload::Hardware { .. } = entry.payload {
                if let Err(err) = Self::remove_hardware_breakpoint(ops, &entry) {
                    tracing::warn!(
                        "Failed to remove hardware breakpoint 0x{:016x}: {err}",
                        entry.info.address.value()
                    );
                }
            }
        }
    }

    /// Add a breakpoint based on the request type.
    pub(crate) fn add_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        request: BreakpointRequest,
    ) -> Result<BreakpointId>
    {
        match request {
            BreakpointRequest::Software { address } => Self::install_software_breakpoint(ops, breakpoints, address),
            BreakpointRequest::Hardware { address } => Self::install_hardware_breakpoint(ops, breakpoints, address),
            BreakpointRequest::Watchpoint { .. } => Err(DebuggerError::InvalidArgument(
                "Watchpoints are not yet supported on macOS".to_string(),
            )),
        }
    }

    /// Remove a breakpoint.
    pub(crate) fn remove_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        id: BreakpointId,
    ) -> Result<()>
    {
        let entry = {
            let mut store = breakpoints.lock().unwrap();
            store.remove(id)
        }
        .ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?;

        if entry.info.enabled {
            match entry.info.kind {
                BreakpointKind::Software => Self::restore_software_breakpoint(ops, &entry)?,
                BreakpointKind::Hardware => Self::remove_hardware_breakpoint(ops, &entry)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Enable a breakpoint.
    pub(crate) fn enable_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        id: BreakpointId,
    ) -> Result<()>
    {
        let (kind, address) = {
            let store = breakpoints.lock().unwrap();
            let entry = store.get(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?;
            if entry.info.enabled {
                return Ok(());
            }
            (entry.info.kind, entry.info.address)
        };

        match kind {
            BreakpointKind::Software => {
                let trap = BreakpointManager::software_trap_bytes(ops.architecture())?;
                let written = ops.write_memory(address, &trap)?;
                if written != trap.len() {
                    return Err(DebuggerError::InvalidArgument(format!(
                        "Failed to re-arm breakpoint at 0x{:016x}",
                        address.value()
                    )));
                }
            }
            BreakpointKind::Hardware => {
                // Re-install on all threads
                let mut used_slot = None;
                for &thread in ops.thread_ports() {
                    let slot = registers::set_hardware_breakpoint(thread, address)?;
                    if let Some(s) = used_slot {
                        if s != slot {
                            tracing::warn!("Hardware breakpoint slots inconsistent: {} vs {}", s, slot);
                        }
                    } else {
                        used_slot = Some(slot);
                    }
                }

                if let Some(new_slot) = used_slot {
                    let mut store = breakpoints.lock().unwrap();
                    if let Some(entry) = store.get_mut(id) {
                        entry.payload = BreakpointPayload::Hardware { address, slot: new_slot };
                    }
                }
            }
            _ => return Err(DebuggerError::InvalidArgument("Watchpoints not supported".into())),
        }

        let mut store = breakpoints.lock().unwrap();
        if let Some(entry) = store.get_mut(id) {
            entry.info.state = BreakpointState::Resolved;
            entry.info.enabled = true;
            entry.info.resolved_at = Some(SystemTime::now());
        }
        Ok(())
    }

    /// Disable a breakpoint.
    pub(crate) fn disable_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        id: BreakpointId,
    ) -> Result<()>
    {
        let (kind, address, payload) = {
            let store = breakpoints.lock().unwrap();
            let entry = store.get(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?;
            if !entry.info.enabled {
                return Ok(());
            }
            (entry.info.kind, entry.info.address, entry.payload.clone())
        };

        match kind {
            BreakpointKind::Software => {
                if let BreakpointPayload::Software { original_bytes } = payload {
                    let written = ops.write_memory(address, &original_bytes)?;
                    if written != original_bytes.len() {
                        return Err(DebuggerError::InvalidArgument(format!(
                            "Failed to disable breakpoint at 0x{:016x}",
                            address.value()
                        )));
                    }
                }
            }
            BreakpointKind::Hardware => {
                if let BreakpointPayload::Hardware { slot, .. } = payload {
                    for &thread in ops.thread_ports() {
                        if let Err(e) = registers::clear_hardware_breakpoint(thread, slot) {
                            tracing::warn!("Failed to clear hardware breakpoint: {}", e);
                        }
                    }
                }
            }
            _ => {}
        }

        let mut store = breakpoints.lock().unwrap();
        if let Some(entry) = store.get_mut(id) {
            entry.info.state = BreakpointState::Disabled;
            entry.info.enabled = false;
        }
        Ok(())
    }

    /// Toggle a breakpoint (enable if disabled, disable if enabled).
    pub(crate) fn toggle_breakpoint<Ops: BreakpointOperations>(
        ops: &mut Ops,
        breakpoints: &Arc<Mutex<BreakpointStore>>,
        id: BreakpointId,
    ) -> Result<bool>
    {
        let enabled = {
            let store = breakpoints.lock().unwrap();
            store
                .get(id)
                .ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))?
                .info
                .enabled
        };
        if enabled {
            Self::disable_breakpoint(ops, breakpoints, id)?;
            Ok(false)
        } else {
            Self::enable_breakpoint(ops, breakpoints, id)?;
            Ok(true)
        }
    }

    /// Get information about a breakpoint.
    pub(crate) fn breakpoint_info(breakpoints: &Arc<Mutex<BreakpointStore>>, id: BreakpointId) -> Result<BreakpointInfo>
    {
        let store = breakpoints.lock().unwrap();
        store.info(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))
    }

    /// List all breakpoints.
    pub(crate) fn breakpoints(breakpoints: &Arc<Mutex<BreakpointStore>>) -> Vec<BreakpointInfo>
    {
        breakpoints.lock().unwrap().list()
    }
}
