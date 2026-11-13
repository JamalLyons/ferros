//! # Diagnostics
//!
//! Safe views of Mach state for tests that cannot use `unsafe` themselves: how many
//! port names (and send-right references) the debugger holds in its own IPC space, and
//! which exception ports a task has. Integration tests use them to show that stops do
//! not leak port rights and that detaching restores a target's exception ports.
//!
//! These read process-wide state: other threads of the calling process that create or
//! release ports at the same time change the counts.

use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::ProcessId;
use libc::{c_int, mach_msg_type_number_t, mach_port_t, vm_address_t, vm_size_t};
use mach2::exception_types::EXC_MASK_ALL;
use mach2::kern_return::{KERN_INVALID_NAME, KERN_INVALID_RIGHT, KERN_SUCCESS};
use mach2::port::{MACH_PORT_DEAD, MACH_PORT_NULL, MACH_PORT_RIGHT_SEND};
use mach2::traps::mach_task_self;

use crate::error::MachError;
use crate::exception::get_exception_ports;
use crate::ffi;

/// `MACH_PORT_TYPE_SEND`: the name denotes a send right.
const MACH_PORT_TYPE_SEND: u32 = 1 << (16 + MACH_PORT_RIGHT_SEND);

/// Names and right types of every entry in this task's IPC space.
fn own_port_names() -> Result<Vec<(mach_port_t, u32)>>
{
    let mut names: *mut mach_port_t = std::ptr::null_mut();
    let mut names_count: mach_msg_type_number_t = 0;
    let mut types: *mut u32 = std::ptr::null_mut();
    let mut types_count: mach_msg_type_number_t = 0;
    // SAFETY: all four out-pointers are valid for writes; on success the kernel stores
    // two arrays in our address space, freed below.
    let kr = unsafe { ffi::mach_port_names(mach_task_self(), &mut names, &mut names_count, &mut types, &mut types_count) };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::from(MachError::from(kr)));
    }

    let count = names_count.min(types_count) as usize;
    let entries = if names.is_null() || types.is_null() {
        Vec::new()
    } else {
        // SAFETY: on success both arrays hold at least `count` initialized entries.
        let (names, types) = unsafe {
            (
                std::slice::from_raw_parts(names, count),
                std::slice::from_raw_parts(types, count),
            )
        };
        names.iter().copied().zip(types.iter().copied()).collect()
    };

    for (array, len, size) in [
        (names as vm_address_t, names_count, size_of::<mach_port_t>()),
        (types as vm_address_t, types_count, size_of::<u32>()),
    ] {
        if array != 0 && len != 0 {
            // SAFETY: the kernel allocated this array for us with `len` entries of `size`
            // bytes; it is not used after this.
            unsafe {
                let _ = ffi::vm_deallocate(mach_task_self(), array, (len as usize * size) as vm_size_t);
            }
        }
    }
    Ok(entries)
}

/// Number of port names in the calling process's own IPC space (`mach_port_names` on
/// `mach_task_self()`).
///
/// # Errors
///
/// `DebuggerError` if `mach_port_names` fails.
pub fn port_name_count() -> Result<usize>
{
    own_port_names().map(|names| names.len())
}

/// Sum of the user references of every send right in the calling process's own IPC
/// space.
///
/// Rights to the same port coalesce under one name, so a leaked send right to a port the
/// process already names (a target's task or thread port) shows up here and not in
/// [`port_name_count`].
///
/// # Errors
///
/// `DebuggerError` if `mach_port_names` or `mach_port_get_refs` fails. A name released by
/// another thread between the two calls is skipped.
pub fn send_right_references() -> Result<u64>
{
    let mut total = 0_u64;
    for (name, kind) in own_port_names()? {
        if kind & MACH_PORT_TYPE_SEND == 0 {
            continue;
        }
        let mut refs = 0_u32;
        // SAFETY: `refs` is valid for writes; a stale name only yields an error.
        let kr = unsafe { ffi::mach_port_get_refs(mach_task_self(), name, MACH_PORT_RIGHT_SEND, &mut refs) };
        match kr {
            KERN_SUCCESS => total += u64::from(refs),
            KERN_INVALID_NAME | KERN_INVALID_RIGHT => {}
            _ => return Err(DebuggerError::from(MachError::from(kr))),
        }
    }
    Ok(total)
}

/// The handler of one group of exception types, as seen from the calling process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionHandler
{
    /// No handler (`MACH_PORT_NULL`).
    None,
    /// The handler port was destroyed (`MACH_PORT_DEAD`).
    Dead,
    /// A live port, by the name of the send right this process holds for it.
    Port(u32),
}

/// One group of a task's exception ports: the exception types in `mask` share a
/// handler, behavior and thread state flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionPortInfo
{
    /// Exception mask (`EXC_MASK_*` bits) of the group.
    pub mask: u32,
    /// Handler port.
    pub handler: ExceptionHandler,
    /// Exception behavior (`EXCEPTION_*`, possibly with `MACH_EXCEPTION_CODES`).
    pub behavior: i32,
    /// Thread state flavor sent with the exception.
    pub flavor: i32,
}

/// A snapshot of a task's exception ports for `EXC_MASK_ALL`, from [`exception_ports`].
///
/// ## Comparing snapshots
///
/// Two snapshots are equal when their groups have the same masks, behaviors, flavors and
/// handlers, where a handler is `None`, `Dead` or a live port by name. The snapshot keeps
/// the send right of every live handler until it is dropped, and send rights to the same
/// port share one name in a process, so while both snapshots are alive equal names mean
/// the same port and different names mean different ports. (A name could be reused for
/// another port once its last right is released, which is why the rights are kept.)
#[derive(Debug)]
pub struct ExceptionPorts
{
    entries: Vec<ExceptionPortInfo>,
}

impl ExceptionPorts
{
    /// The groups, in the order the kernel reports them.
    #[must_use]
    pub fn entries(&self) -> &[ExceptionPortInfo]
    {
        &self.entries
    }
}

impl PartialEq for ExceptionPorts
{
    fn eq(&self, other: &Self) -> bool
    {
        self.entries == other.entries
    }
}

impl Eq for ExceptionPorts {}

impl Drop for ExceptionPorts
{
    fn drop(&mut self)
    {
        for entry in &self.entries {
            if let ExceptionHandler::Port(name) = entry.handler {
                // SAFETY: the snapshot owns one user reference of `name`, released once.
                unsafe {
                    let _ = ffi::mach_port_deallocate(mach_task_self(), name);
                }
            }
        }
    }
}

/// Snapshot the exception ports of process `pid` for `EXC_MASK_ALL`
/// (`task_for_pid` + `task_get_exception_ports`). See [`ExceptionPorts`] for what
/// comparing two snapshots means.
///
/// Needs the same permission as attaching. The task port is released before returning.
///
/// # Errors
///
/// `DebuggerError` if `task_for_pid` or `task_get_exception_ports` fails.
pub fn exception_ports(pid: ProcessId) -> Result<ExceptionPorts>
{
    let mut task: mach_port_t = MACH_PORT_NULL;
    // SAFETY: `task` is valid for writes.
    let kr = unsafe { ffi::task_for_pid(mach_task_self(), pid.0 as c_int, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::from(MachError::from(kr)));
    }
    let entries = get_exception_ports(task, EXC_MASK_ALL);
    // SAFETY: `task` is the send right task_for_pid gave us, released once.
    unsafe {
        let _ = ffi::mach_port_deallocate(mach_task_self(), task);
    }
    let entries = entries.map_err(|kr| DebuggerError::from(MachError::from(kr)))?;
    Ok(ExceptionPorts {
        entries: entries
            .into_iter()
            .map(|entry| ExceptionPortInfo {
                mask: entry.mask,
                handler: match entry.handler {
                    MACH_PORT_NULL => ExceptionHandler::None,
                    MACH_PORT_DEAD => ExceptionHandler::Dead,
                    name => ExceptionHandler::Port(name),
                },
                behavior: entry.behavior,
                flavor: entry.flavor,
            })
            .collect(),
    })
}
