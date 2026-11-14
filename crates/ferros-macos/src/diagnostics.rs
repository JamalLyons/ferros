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
use ferros_core::types::{Address, ProcessId};
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

/// User references of the send right named `name` in the calling process's own IPC
/// space. A name that is not (or no longer) a send right counts 0.
///
/// # Errors
///
/// `DebuggerError` if `mach_port_get_refs` fails for another reason.
pub fn send_right_count(name: u32) -> Result<u32>
{
    let mut refs = 0_u32;
    // SAFETY: `refs` is valid for writes; a stale name only yields an error.
    let kr = unsafe { ffi::mach_port_get_refs(mach_task_self(), name, MACH_PORT_RIGHT_SEND, &mut refs) };
    match kr {
        KERN_SUCCESS => Ok(refs),
        KERN_INVALID_NAME | KERN_INVALID_RIGHT => Ok(0),
        _ => Err(DebuggerError::from(MachError::from(kr))),
    }
}

/// Read `len` bytes of process `pid` exactly as they are in memory, through a task port
/// of its own (`task_for_pid`).
///
/// The backend's `read_memory` hides the traps it plants for itself; this shows them.
/// Needs the same permission as attaching. The task port is released before returning.
///
/// # Errors
///
/// `DebuggerError` if `task_for_pid` or the read fails.
pub fn raw_memory(pid: ProcessId, address: Address, len: usize) -> Result<Vec<u8>>
{
    let mut task: mach_port_t = MACH_PORT_NULL;
    // SAFETY: `task` is valid for writes.
    let kr = unsafe { ffi::task_for_pid(mach_task_self(), pid.0 as c_int, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::from(MachError::from(kr)));
    }
    let bytes = crate::memory::read_memory(task, address, len);
    // SAFETY: `task` is the send right task_for_pid gave us, released once.
    unsafe {
        let _ = ffi::mach_port_deallocate(mach_task_self(), task);
    }
    bytes
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

/// Signal-handler observation used by the SIGCONT integration fixture. The handler
/// only performs a lock-free atomic store; printing happens on the fixture's main thread.
static SIGCONT_OBSERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn observe_sigcont(_: libc::c_int)
{
    SIGCONT_OBSERVED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Install the integration fixture's SIGCONT observer in the calling process.
/// Not part of the supported API.
///
/// # Errors
/// Returns the OS error if the handler could not be installed.
#[doc(hidden)]
pub fn install_sigcont_observer() -> std::io::Result<()>
{
    // SAFETY: the static handler has C ABI and performs only a lock-free atomic store.
    if unsafe { libc::signal(libc::SIGCONT, observe_sigcont as *const () as libc::sighandler_t) } == libc::SIG_ERR {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether the fixture's SIGCONT handler has run. Not part of the supported API.
#[doc(hidden)]
#[must_use]
pub fn sigcont_observed() -> bool
{
    SIGCONT_OBSERVED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Install the SIGUSR1 observer used by the signal-tracing fixture. Tests only.
#[doc(hidden)]
pub fn install_sigusr1_observer() -> std::io::Result<()>
{
    // SAFETY: the static handler only performs an atomic store.
    if unsafe { libc::signal(libc::SIGUSR1, observe_sigusr1 as *const () as libc::sighandler_t) } == libc::SIG_ERR {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
static SIGUSR1_OBSERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn observe_sigusr1(_: libc::c_int)
{
    use std::sync::atomic::Ordering;
    SIGUSR1_OBSERVED.store(true, Ordering::Relaxed);
    // SAFETY: pthread_self only reads the calling thread identity. The handler
    // otherwise performs lock-free atomic accesses, never allocates or locks.
    let current = unsafe { libc::pthread_self() } as usize;
    for (slot, thread) in SIGNAL_THREADS.iter().enumerate() {
        if thread.load(Ordering::Relaxed) == current {
            SIGNAL_HANDLED.fetch_or(1 << slot, Ordering::Relaxed);
        }
    }
}
/// Whether the fixture handled SIGUSR1. Tests only.
#[doc(hidden)]
#[must_use]
pub fn sigusr1_observed() -> bool
{
    SIGUSR1_OBSERVED.load(std::sync::atomic::Ordering::Relaxed)
}

static SIGNAL_THREADS: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];
static SIGNAL_HANDLED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Register a fixture worker's identity for the SIGUSR1 observer. Tests only.
#[doc(hidden)]
pub fn register_signal_worker(slot: usize)
{
    // SAFETY: pthread_self returns the calling thread's opaque identity.
    SIGNAL_THREADS[slot].store(unsafe { libc::pthread_self() } as usize, std::sync::atomic::Ordering::Relaxed);
}

/// Bitset of registered workers whose SIGUSR1 handler ran. Tests only.
#[doc(hidden)]
#[must_use]
pub fn signal_workers_handled() -> usize
{
    SIGNAL_HANDLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Raise SIGUSR1 on the calling fixture worker. Tests only.
#[doc(hidden)]
pub fn raise_sigusr1() -> std::io::Result<()>
{
    // SAFETY: SIGUSR1 is installed by the fixture before workers start.
    if unsafe { libc::raise(libc::SIGUSR1) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Read MDSCR_EL1.SS using a thread port held by the debugger. Tests only.
///
/// # Errors
/// Returns the Mach error if the name is invalid or the state cannot be read.
#[doc(hidden)]
pub fn single_step_enabled(thread: ferros_core::ThreadId) -> Result<bool>
{
    #[cfg(target_arch = "aarch64")]
    {
        crate::registers::single_step_enabled(thread.raw() as mach_port_t)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = thread;
        Err(DebuggerError::InvalidArgument("single-step diagnostics require arm64".into()))
    }
}

/// Read THREAD_BASIC_INFO's per-thread suspend count, excluding task suspensions.
/// The debugger must still own the supplied thread port. Tests only.
///
/// # Errors
/// Returns the Mach error if the thread info cannot be read.
#[doc(hidden)]
pub fn thread_suspend_count(thread: ferros_core::ThreadId) -> Result<i32>
{
    thread_basic_state(thread).map(|info| info.suspend_count)
}

/// A thread's basic Mach scheduling state, sampled without suspending it. Tests only.
#[doc(hidden)]
#[derive(Debug)]
pub struct ThreadBasicState
{
    /// Per-thread suspend count, excluding task suspensions.
    pub suspend_count: i32,
    /// Mach THREAD_BASIC_INFO run_state.
    pub run_state: i32,
    /// Mach THREAD_BASIC_INFO flags.
    pub flags: i32,
}

fn thread_basic_state(thread: ferros_core::ThreadId) -> Result<ThreadBasicState>
{
    // `struct thread_basic_info`: user_time and system_time (two time_value_t pairs),
    // then cpu_usage, policy, run_state, flags, suspend_count and sleep_time.
    const THREAD_BASIC_INFO: libc::natural_t = 3;
    const THREAD_BASIC_INFO_COUNT: mach_msg_type_number_t = 10;
    const SUSPEND_COUNT: usize = 8;
    let mut info = [0_i32; THREAD_BASIC_INFO_COUNT as usize];
    let mut count = THREAD_BASIC_INFO_COUNT;
    // SAFETY: `info` holds exactly THREAD_BASIC_INFO_COUNT integer_t words.
    let kr = unsafe { ffi::thread_info(thread.raw() as mach_port_t, THREAD_BASIC_INFO, info.as_mut_ptr(), &mut count) };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::from(MachError::from(kr)));
    }
    if (count as usize) <= SUSPEND_COUNT {
        return Err(DebuggerError::InvalidArgument(format!(
            "thread_info returned {count} words, too few for suspend_count"
        )));
    }
    Ok(ThreadBasicState {
        suspend_count: info[SUSPEND_COUNT],
        run_state: info[6],
        flags: info[7],
    })
}

/// Independent task and per-thread samples; errors preserve other successful samples.
#[doc(hidden)]
#[derive(Debug)]
pub struct ProcessSuspendCounts
{
    /// Task suspend count, including any BSD job-control suspension.
    pub task: Result<i32>,
    /// Per-thread samples, or the thread-enumeration error.
    pub threads: Result<Vec<Result<ThreadBasicState>>>,
}

/// Snapshot task and per-thread suspend counts without attaching or stopping the
/// process. Used after release timeouts, before cleanup destroys the evidence.
/// Counts are sampled separately and can change while the process runs. Tests only.
///
/// # Errors
/// Returns an error if the task port cannot be acquired. Individual sampling errors
/// are retained in the snapshot.
#[doc(hidden)]
pub fn process_suspend_counts(pid: ProcessId) -> Result<ProcessSuspendCounts>
{
    use mach2::task::task_info;
    use mach2::task_info::{MACH_TASK_BASIC_INFO, MACH_TASK_BASIC_INFO_COUNT, mach_task_basic_info};

    let mut task = MACH_PORT_NULL;
    // SAFETY: `task` is writable; the acquired send right is released below.
    let kr = unsafe { ffi::task_for_pid(mach_task_self(), pid.0 as c_int, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::from(MachError::from(kr)));
    }
    let result = {
        let mut info = mach_task_basic_info::default();
        let mut count = MACH_TASK_BASIC_INFO_COUNT;
        // SAFETY: `info` holds MACH_TASK_BASIC_INFO_COUNT words and outlives the call.
        let kr = unsafe { task_info(task, MACH_TASK_BASIC_INFO, (&raw mut info).cast(), &mut count) };
        let task_count = if kr == KERN_SUCCESS {
            Ok(info.suspend_count)
        } else {
            Err(DebuggerError::from(MachError::from(kr)))
        };
        let counts = crate::threads::ThreadManager::refresh_thread_list(task, &[]).map(|threads| {
            let counts = threads
                .iter()
                .map(|&port| thread_basic_state(ferros_core::ThreadId::from(u64::from(port))))
                .collect();
            for port in threads {
                // SAFETY: refresh_thread_list returned one owned send right per entry.
                unsafe {
                    let _ = ffi::mach_port_deallocate(mach_task_self(), port);
                }
            }
            counts
        });
        Ok(ProcessSuspendCounts {
            task: task_count,
            threads: counts,
        })
    };
    // SAFETY: release the task_for_pid send right exactly once, on all paths.
    unsafe {
        let _ = ffi::mach_port_deallocate(mach_task_self(), task);
    }
    result
}

/// The calling thread's system-wide unique id (`pthread_threadid_np`), for fixtures
/// that report which worker is which. Tests only.
#[doc(hidden)]
#[must_use]
pub fn current_thread_unique_id() -> u64
{
    let mut id = 0_u64;
    // SAFETY: pthread_self names the calling thread; `id` is valid for writes.
    let rc = unsafe { libc::pthread_threadid_np(libc::pthread_self(), &mut id) };
    assert_eq!(rc, 0, "pthread_threadid_np failed");
    id
}

/// The system-wide unique id of a target thread whose port the debugger holds
/// (`THREAD_IDENTIFIER_INFO`), matching [`current_thread_unique_id`] in the target.
/// Tests only.
///
/// # Errors
/// Returns the Mach error if the thread info cannot be read.
#[doc(hidden)]
pub fn thread_unique_id(thread: ferros_core::ThreadId) -> Result<u64>
{
    // `struct thread_identifier_info`: thread_id, thread_handle, dispatch_qaddr (u64 each).
    const THREAD_IDENTIFIER_INFO: libc::natural_t = 4;
    const THREAD_IDENTIFIER_INFO_COUNT: mach_msg_type_number_t = 6;
    let mut info = [0_u64; 3];
    let mut count = THREAD_IDENTIFIER_INFO_COUNT;
    // SAFETY: `info` is 24 bytes, exactly THREAD_IDENTIFIER_INFO_COUNT natural_t words.
    let kr = unsafe {
        ffi::thread_info(
            thread.raw() as mach_port_t,
            THREAD_IDENTIFIER_INFO,
            info.as_mut_ptr().cast::<c_int>(),
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(DebuggerError::from(MachError::from(kr)));
    }
    if count < 2 {
        return Err(DebuggerError::InvalidArgument(format!(
            "thread_info returned {count} words, too few for thread_id"
        )));
    }
    Ok(info[0])
}
