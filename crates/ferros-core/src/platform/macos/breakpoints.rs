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
//! - [registers::debug] for hardware breakpoint implementation
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
/// and thread lists without exposing the entire `MacOSDebugger` struct. It provides
/// a clean interface for breakpoint management while maintaining encapsulation.
pub(crate) trait BreakpointOperations
{
    /// Read memory from the target process.
    ///
    /// ## Parameters
    ///
    /// - `addr`: The address to read from
    /// - `len`: The number of bytes to read
    ///
    /// ## Returns
    ///
    /// `Ok(Vec<u8>)` containing the read bytes, or an error if the read fails.
    fn read_memory(&self, addr: Address, len: usize) -> Result<Vec<u8>>;

    /// Write memory to the target process.
    ///
    /// ## Parameters
    ///
    /// - `addr`: The address to write to
    /// - `data`: The bytes to write
    ///
    /// ## Returns
    ///
    /// `Ok(usize)` with the number of bytes written, or an error if the write fails.
    fn write_memory(&mut self, addr: Address, data: &[u8]) -> Result<usize>;

    /// Get the list of thread ports for the target process.
    ///
    /// Returns a slice of Mach thread ports (`thread_act_t`) representing all threads
    /// in the debugged process. Hardware breakpoints are installed on all threads
    /// to ensure consistent behavior.
    ///
    /// ## Returns
    ///
    /// A slice of thread ports. The slice may be empty if the process has no threads.
    fn thread_ports(&self) -> &[thread_act_t];

    /// Get the target CPU architecture.
    ///
    /// This is used to determine which trap instruction to use for software breakpoints
    /// and which debug registers are available for hardware breakpoints.
    ///
    /// ## Returns
    ///
    /// The `Architecture` of the target process (Arm64, X86_64, or Unknown).
    fn architecture(&self) -> Architecture;

    /// Ensure the debugger is attached to a process.
    ///
    /// This method verifies that the debugger is currently attached to a process.
    /// Breakpoint operations require an attached process to function correctly.
    ///
    /// ## Returns
    ///
    /// `Ok(())` if attached, or an error if not attached.
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::AttachFailed`: Debugger is not attached to a process
    fn ensure_attached(&self) -> Result<()>;
}

/// Breakpoint management functions for macOS debugger.
///
/// This struct provides static methods for managing breakpoints in a debugged process.
/// It handles both software and hardware breakpoints, including installation, removal,
/// enabling, disabling, and querying breakpoint state.
///
/// ## Breakpoint Lifecycle
///
/// 1. **Install**: Create a new breakpoint at an address
/// 2. **Enable/Disable**: Temporarily activate or deactivate a breakpoint
/// 3. **Remove**: Permanently delete a breakpoint and restore original code
///
/// ## Thread Safety
///
/// Breakpoint operations are thread-safe when used with a shared `BreakpointStore`
/// protected by a `Mutex`. Hardware breakpoints are installed on all threads in
/// the process to ensure consistent behavior.
pub(crate) struct BreakpointManager;

impl BreakpointManager
{
    /// Get the trap instruction bytes for the current architecture.
    ///
    /// Returns the instruction sequence that triggers a breakpoint when executed:
    /// - **ARM64**: `BRK #0` instruction (4 bytes: `0x00, 0x00, 0x20, 0xD4`)
    /// - **x86-64**: `INT3` instruction (1 byte: `0xCC`)
    ///
    /// ## Parameters
    ///
    /// - `architecture`: The target CPU architecture
    ///
    /// ## Returns
    ///
    /// `Ok(Vec<u8>)` containing the trap instruction bytes, or an error if:
    /// - The architecture is not supported
    /// - The build does not support the requested architecture
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::InvalidArgument`: Architecture not supported or not available in this build
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
    /// This method replaces the instruction at `address` with a trap instruction
    /// (INT3 on x86-64, BRK on ARM64). The original instruction bytes are saved
    /// so they can be restored later when the breakpoint is removed or disabled.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory access and architecture info
    /// - `breakpoints`: Shared breakpoint store for tracking breakpoints
    /// - `address`: Memory address where the breakpoint should be installed
    ///
    /// ## Returns
    ///
    /// `Ok(BreakpointId)` with the ID of the newly created breakpoint, or an error if:
    /// - A breakpoint already exists at this address
    /// - The address cannot be read or written
    /// - The debugger is not attached to a process
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::InvalidArgument`: Breakpoint already exists or memory access failed
    /// - `DebuggerError::AttachFailed`: Debugger is not attached to a process
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
    /// This method uses CPU debug registers to break on instruction execution.
    /// Hardware breakpoints are installed on all threads in the process to ensure
    /// consistent behavior across all execution contexts.
    ///
    /// ## Hardware Breakpoint Limitations
    ///
    /// - **x86-64**: Limited to 4 hardware breakpoints (DR0-DR7 registers)
    /// - **ARM64**: Limited to 16 hardware breakpoints (DBGBVR/DBGBCR registers)
    ///
    /// If all available slots are in use, this method will return an error.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing thread access and architecture info
    /// - `breakpoints`: Shared breakpoint store for tracking breakpoints
    /// - `address`: Memory address where the breakpoint should be installed
    ///
    /// ## Returns
    ///
    /// `Ok(BreakpointId)` with the ID of the newly created breakpoint, or an error if:
    /// - A hardware breakpoint already exists at this address
    /// - All hardware breakpoint slots are in use
    /// - No threads are available in the process
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::InvalidArgument`: Breakpoint already exists
    /// - `DebuggerError::AttachFailed`: No threads available or slot allocation failed
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
    ///
    /// This method replaces the trap instruction with the original instruction bytes
    /// that were saved when the breakpoint was installed. This is used when removing
    /// or temporarily disabling a software breakpoint.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory write access
    /// - `entry`: The breakpoint entry containing the original instruction bytes
    ///
    /// ## Returns
    ///
    /// `Ok(())` if the original instruction was successfully restored, or an error
    /// if memory write failed.
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::InvalidArgument`: Failed to write original instruction to memory
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
    ///
    /// This method clears the hardware breakpoint from all threads in the process
    /// by resetting the debug register slot. The operation is best-effort: if clearing
    /// fails on some threads, warnings are logged but the function still returns `Ok(())`.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing thread access
    /// - `entry`: The breakpoint entry containing the slot number to clear
    ///
    /// ## Returns
    ///
    /// `Ok(())` always. Failures to clear breakpoints on individual threads are logged
    /// as warnings but do not cause the function to return an error.
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
    ///
    /// This method removes all breakpoints from the process by:
    /// - Restoring original instructions for software breakpoints
    /// - Clearing debug registers for hardware breakpoints
    ///
    /// This is typically called when detaching from a process or cleaning up
    /// before process termination. Failures to restore individual breakpoints
    /// are logged as warnings but do not stop the process.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory and thread access
    /// - `breakpoints`: Shared breakpoint store containing all breakpoints
    ///
    /// ## Note
    ///
    /// This method drains all breakpoints from the store. After calling this,
    /// the breakpoint store will be empty.
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
            } else if let BreakpointPayload::Hardware { .. } = entry.payload
                && let Err(err) = Self::remove_hardware_breakpoint(ops, &entry)
            {
                tracing::warn!(
                    "Failed to remove hardware breakpoint 0x{:016x}: {err}",
                    entry.info.address.value()
                );
            }
        }
    }

    /// Add a breakpoint based on the request type.
    ///
    /// This is a convenience method that dispatches to the appropriate installation
    /// method based on the breakpoint request type (software, hardware, or watchpoint).
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory and thread access
    /// - `breakpoints`: Shared breakpoint store for tracking breakpoints
    /// - `request`: The breakpoint request specifying type and address
    ///
    /// ## Returns
    ///
    /// `Ok(BreakpointId)` with the ID of the newly created breakpoint, or an error if:
    /// - The breakpoint type is not supported (e.g., watchpoints)
    /// - Installation fails for any reason
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::InvalidArgument`: Watchpoints not supported, or installation failed
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

    /// Remove a breakpoint permanently.
    ///
    /// This method removes a breakpoint from the store and restores the original
    /// instruction (for software breakpoints) or clears the debug register (for
    /// hardware breakpoints). The breakpoint is completely removed and cannot
    /// be re-enabled.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory and thread access
    /// - `breakpoints`: Shared breakpoint store containing the breakpoint
    /// - `id`: The ID of the breakpoint to remove
    ///
    /// ## Returns
    ///
    /// `Ok(())` if the breakpoint was successfully removed, or an error if:
    /// - The breakpoint ID does not exist
    /// - Restoring the original instruction failed
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::BreakpointIdNotFound`: The breakpoint ID does not exist
    /// - `DebuggerError::InvalidArgument`: Failed to restore original instruction
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
    ///
    /// This method activates a previously disabled breakpoint by:
    /// - Writing the trap instruction back to memory (for software breakpoints)
    /// - Re-installing the breakpoint in debug registers (for hardware breakpoints)
    ///
    /// If the breakpoint is already enabled, this method returns `Ok(())` without
    /// performing any operations.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory and thread access
    /// - `breakpoints`: Shared breakpoint store containing the breakpoint
    /// - `id`: The ID of the breakpoint to enable
    ///
    /// ## Returns
    ///
    /// `Ok(())` if the breakpoint was successfully enabled, or an error if:
    /// - The breakpoint ID does not exist
    /// - Writing the trap instruction failed (software breakpoints)
    /// - Installing the debug register failed (hardware breakpoints)
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::BreakpointIdNotFound`: The breakpoint ID does not exist
    /// - `DebuggerError::InvalidArgument`: Failed to enable the breakpoint
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
    ///
    /// This method temporarily deactivates a breakpoint by:
    /// - Restoring the original instruction (for software breakpoints)
    /// - Clearing the debug register (for hardware breakpoints)
    ///
    /// The breakpoint remains in the store and can be re-enabled later. If the
    /// breakpoint is already disabled, this method returns `Ok(())` without
    /// performing any operations.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory and thread access
    /// - `breakpoints`: Shared breakpoint store containing the breakpoint
    /// - `id`: The ID of the breakpoint to disable
    ///
    /// ## Returns
    ///
    /// `Ok(())` if the breakpoint was successfully disabled, or an error if:
    /// - The breakpoint ID does not exist
    /// - Restoring the original instruction failed (software breakpoints)
    /// - Clearing the debug register failed (hardware breakpoints)
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::BreakpointIdNotFound`: The breakpoint ID does not exist
    /// - `DebuggerError::InvalidArgument`: Failed to disable the breakpoint
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
    ///
    /// This convenience method checks the current state of a breakpoint and toggles
    /// it to the opposite state. It's useful for implementing UI controls that
    /// allow users to quickly enable/disable breakpoints.
    ///
    /// ## Parameters
    ///
    /// - `ops`: Operations trait providing memory and thread access
    /// - `breakpoints`: Shared breakpoint store containing the breakpoint
    /// - `id`: The ID of the breakpoint to toggle
    ///
    /// ## Returns
    ///
    /// `Ok(true)` if the breakpoint was enabled, `Ok(false)` if it was disabled,
    /// or an error if the operation failed.
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::BreakpointIdNotFound`: The breakpoint ID does not exist
    /// - `DebuggerError::InvalidArgument`: Failed to toggle the breakpoint
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
    ///
    /// This method retrieves the metadata for a breakpoint, including its address,
    /// kind, state, and whether it's enabled. This is useful for displaying breakpoint
    /// information in a debugger UI or for programmatic inspection.
    ///
    /// ## Parameters
    ///
    /// - `breakpoints`: Shared breakpoint store containing the breakpoint
    /// - `id`: The ID of the breakpoint to query
    ///
    /// ## Returns
    ///
    /// `Ok(BreakpointInfo)` containing the breakpoint metadata, or an error if
    /// the breakpoint ID does not exist.
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::BreakpointIdNotFound`: The breakpoint ID does not exist
    pub(crate) fn breakpoint_info(breakpoints: &Arc<Mutex<BreakpointStore>>, id: BreakpointId) -> Result<BreakpointInfo>
    {
        let store = breakpoints.lock().unwrap();
        store.info(id).ok_or_else(|| DebuggerError::BreakpointIdNotFound(id.raw()))
    }

    /// List all breakpoints.
    ///
    /// This method returns a vector containing information about all breakpoints
    /// currently registered in the breakpoint store. This is useful for displaying
    /// a list of all breakpoints in a debugger UI.
    ///
    /// ## Parameters
    ///
    /// - `breakpoints`: Shared breakpoint store to query
    ///
    /// ## Returns
    ///
    /// A vector of `BreakpointInfo` structures, one for each breakpoint in the store.
    /// The vector is empty if no breakpoints are registered.
    pub(crate) fn breakpoints(breakpoints: &Arc<Mutex<BreakpointStore>>) -> Vec<BreakpointInfo>
    {
        breakpoints.lock().unwrap().list()
    }
}
