//! # macOS Memory Operations
//!
//! Memory reading and writing using Mach APIs.
//!
//! On macOS, we use `vm_read()` and `vm_write()` to access process memory.
//! These are Mach APIs that work with task ports obtained from `task_for_pid()`.

use std::cmp::min;
use std::io::{Error, ErrorKind};

use libc::{c_int, mach_port_t, vm_address_t, vm_map_t, vm_offset_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
use mach2::message::mach_msg_type_number_t;
#[cfg(target_os = "macos")]
use mach2::vm::{mach_vm_protect, mach_vm_read_overwrite, mach_vm_region_recurse};
#[cfg(target_os = "macos")]
use mach2::vm_region::{
    vm_region_recurse_info_t, vm_region_submap_short_info_data_64_t, VM_REGION_SUBMAP_SHORT_INFO_COUNT_64,
};
#[cfg(target_os = "macos")]
use mach2::vm_statistics::{
    VM_MEMORY_MALLOC, VM_MEMORY_MALLOC_HUGE, VM_MEMORY_MALLOC_LARGE, VM_MEMORY_MALLOC_MEDIUM, VM_MEMORY_MALLOC_SMALL,
    VM_MEMORY_MALLOC_TINY, VM_MEMORY_STACK,
};
#[cfg(target_os = "macos")]
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t, natural_t};

use crate::error::{DebuggerError, Result};
use crate::platform::macos::ffi;
use crate::types::{Address, MemoryRegion, MemoryRegionId};

const MAX_VM_READ_CHUNK: usize = 64 * 1024;

/// Read memory from a Mach task
///
/// Uses `mach_vm_read_overwrite()` to read memory from the target process in bounded chunks.
/// The data is streamed directly into the returned `Vec<u8>` to avoid transient Mach allocations.
///
/// ## Parameters
///
/// - `task`: Mach task port obtained from `task_for_pid()`
/// - `addr`: Virtual address in the target process to read from
/// - `len`: Number of bytes to read
///
/// ## Returns
///
/// A `Vec<u8>` containing the read bytes. The actual number of bytes read may be less
/// than requested if the address range is not fully accessible.
///
/// ## Errors
///
/// Returns an `Io` error if:
/// - The address is invalid or not accessible
/// - Insufficient permissions to read the memory
/// - The memory region is not readable
///
/// ## Safety
///
/// This function is marked `unsafe` internally because it calls the Mach API `vm_read()`.
/// The function itself is safe to call - all safety checks are performed internally.
///
/// ## Mach API: vm_read()
///
/// ```c
/// kern_return_t vm_read(
///     vm_map_t target_task,    // Task port from task_for_pid()
///     vm_address_t address,    // Address to read from
///     vm_size_t size,          // Number of bytes to read
///     vm_offset_t *data,       // Output: pointer to data
///     mach_msg_type_number_t *data_count // Output: actual bytes read
/// );
/// ```
///
/// See: [vm_read(3) man page](https://developer.apple.com/documentation/kernel/1585350-vm_read/)
pub fn read_memory(task: mach_port_t, addr: Address, len: usize) -> Result<Vec<u8>>
{
    if len == 0 {
        return Ok(Vec::new());
    }

    let mut buffer = vec![0u8; len];
    let bytes_read = read_memory_into(task, addr, &mut buffer)?;
    buffer.truncate(bytes_read);
    Ok(buffer)
}

/// Write memory to a Mach task
///
/// Uses `vm_write()` to write memory to the target process. The data is copied from
/// the current process's address space into the target process's address space.
///
/// ## ⚠️ Warning
///
/// Writing to memory can crash the target process or cause undefined behavior.
/// Only write to writable memory regions (e.g., stack, heap). Writing to code
/// segments may corrupt the program or cause crashes.
///
/// ## Parameters
///
/// - `task`: Mach task port obtained from `task_for_pid()`
/// - `addr`: Virtual address in the target process to write to
/// - `data`: Slice of bytes to write
///
/// ## Returns
///
/// The number of bytes written (should equal `data.len()` on success).
///
/// ## Errors
///
/// Returns an `Io` error if:
/// - The address is invalid or not accessible
/// - Insufficient permissions to write the memory
/// - The memory region is read-only
/// - The write would cross a memory region boundary
///
/// ## Safety
///
/// This function is marked `unsafe` internally because it calls the Mach API `vm_write()`.
/// The function itself is safe to call - all safety checks are performed internally.
///
/// ## Mach API: vm_write()
///
/// ```c
/// kern_return_t vm_write(
///     vm_map_t target_task,    // Task port from task_for_pid()
///     vm_address_t address,    // Address to write to
///     vm_offset_t data,        // Pointer to data to write
///     mach_msg_type_number_t data_count // Number of bytes to write
/// );
/// ```
///
/// See: [vm_write(3) man page](https://developer.apple.com/documentation/kernel/1585462-vm_write/)
pub fn write_memory(task: mach_port_t, addr: Address, data: &[u8]) -> Result<usize>
{
    if data.is_empty() {
        return Ok(0);
    }

    ensure_range(task, addr, data.len())?;

    unsafe {
        let result = ffi::vm_write(
            task as vm_map_t,
            addr.value() as vm_address_t,
            data.as_ptr() as vm_offset_t,
            data.len() as mach_msg_type_number_t,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("vm_write() failed with error code: {}", result),
            )));
        }

        Ok(data.len())
    }
}

/// Get memory regions for a Mach task
///
/// Uses `mach_vm_region()` to enumerate all memory regions in the target process.
/// This iterates through the entire virtual address space, collecting information
/// about each contiguous memory region (code segments, data segments, stack, heap, etc.).
///
/// ## Parameters
///
/// - `task`: Mach task port obtained from `task_for_pid()`
///
/// ## Returns
///
/// A vector of `MemoryRegion` structs, each representing a contiguous memory region
/// with its start address, end address, permissions, and optional name.
///
/// ## Errors
///
/// Returns an `Io` error if:
/// - The task port is invalid
/// - Insufficient permissions to query memory regions
///
/// ## Note
///
/// On macOS 10.5+, `vm_region` was replaced by `mach_vm_region` for 64-bit applications.
/// This function uses `mach_vm_region` which is the modern API.
///
/// ## Implementation Details
///
/// The function iterates through memory regions by:
/// 1. Starting at address 0
/// 2. Calling `mach_vm_region()` to get information about the region at the current address
/// 3. Converting protection flags to a permission string (r/w/x)
/// 4. Moving to the next address (current address + region size)
/// 5. Repeating until `mach_vm_region()` returns an error (end of address space)
///
/// See: [mach_vm_region(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_region/)
pub fn get_memory_regions(task: mach_port_t) -> Result<Vec<MemoryRegion>>
{
    let mut regions = Vec::new();
    let mut address: mach_vm_address_t = 0;
    let mut region_id = 0usize;
    let mut depth: natural_t = 0;

    unsafe {
        loop {
            let mut size: mach_vm_size_t = 0;
            let mut info = vm_region_submap_short_info_data_64_t::default();
            let mut info_count = VM_REGION_SUBMAP_SHORT_INFO_COUNT_64;

            let result = mach_vm_region_recurse(
                task as vm_map_t,
                &mut address,
                &mut size,
                &mut depth,
                &mut info as *mut _ as vm_region_recurse_info_t,
                &mut info_count,
            );

            if result == libc::KERN_INVALID_ADDRESS {
                break;
            }
            if result != KERN_SUCCESS {
                return Err(DebuggerError::Io(Error::other(format!(
                    "mach_vm_region_recurse failed: {}",
                    result
                ))));
            }

            if info.is_submap != 0 {
                depth += 1;
                continue;
            }

            let perms = protection_to_permissions(info.protection);
            // Try to get name from user_tag first, then fall back to heuristics
            let name = region_name_from_user_tag(info.user_tag).or_else(|| region_name_from_heuristics(info.protection));

            regions.push(MemoryRegion::new(
                MemoryRegionId(region_id),
                Address::from(address),
                Address::from(address + size),
                perms,
                name,
            ));
            region_id += 1;

            // Move to the next memory region
            // The next region starts right after this one ends
            address += size;
        }
    }

    Ok(regions)
}

/// Read memory directly into an existing buffer without allocating an intermediate Mach buffer.
pub fn read_memory_into(task: mach_port_t, addr: Address, dst: &mut [u8]) -> Result<usize>
{
    if dst.is_empty() {
        return Ok(0);
    }

    ensure_readable_range(task, addr, dst.len())?;

    let mut total = 0usize;
    let mut cursor = addr.value();

    while total < dst.len() {
        let chunk_len = min(MAX_VM_READ_CHUNK, dst.len() - total);
        let mut actual: mach_vm_size_t = 0;

        let result = unsafe {
            mach_vm_read_overwrite(
                task as vm_map_t,
                cursor,
                chunk_len as mach_vm_size_t,
                dst[total..].as_mut_ptr() as mach_vm_address_t,
                &mut actual,
            )
        };

        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(Error::other(format!(
                "mach_vm_read_overwrite failed: {}",
                result
            ))));
        }

        if actual == 0 {
            break;
        }

        total += actual as usize;
        cursor += actual;
    }

    Ok(total)
}

/// Guard that temporarily changes the protection of a memory range and restores it on drop.
pub struct MemoryProtectionGuard
{
    task: mach_port_t,
    addr: mach_vm_address_t,
    len: mach_vm_size_t,
    original: c_int,
    active: bool,
}

impl MemoryProtectionGuard
{
    /// Make the specified range writable (and readable/executable) until the guard is dropped.
    pub fn make_writable(task: mach_port_t, addr: Address, len: usize) -> Result<Self>
    {
        Self::with_protection(
            task,
            addr,
            len,
            libc::VM_PROT_READ | libc::VM_PROT_WRITE | libc::VM_PROT_EXECUTE,
        )
    }

    /// Apply an arbitrary protection mask for the lifetime of the guard.
    pub fn with_protection(task: mach_port_t, addr: Address, len: usize, protection: c_int) -> Result<Self>
    {
        if len == 0 {
            return Ok(Self {
                task,
                addr: 0,
                len: 0,
                original: 0,
                active: false,
            });
        }

        let region = ensure_range(task, addr, len)?;
        let (aligned_addr, aligned_len) = aligned_range(addr, len);
        let original = region.protection as c_int;

        change_protection(task, aligned_addr, aligned_len, protection)?;

        Ok(Self {
            task,
            addr: aligned_addr,
            len: aligned_len,
            original,
            active: true,
        })
    }
}

impl Drop for MemoryProtectionGuard
{
    fn drop(&mut self)
    {
        if self.active {
            let _ = change_protection(self.task, self.addr, self.len, self.original);
        }
    }
}

#[derive(Debug, Clone)]
struct RegionInfo
{
    start: mach_vm_address_t,
    size: mach_vm_size_t,
    protection: u32,
    #[allow(dead_code)] // Used in TUI for displaying region names
    name: Option<String>,
}

fn ensure_readable_range(task: mach_port_t, addr: Address, len: usize) -> Result<RegionInfo>
{
    ensure_range_with_permissions(task, addr, len, Some(libc::VM_PROT_READ as u32))
}

fn ensure_range(task: mach_port_t, addr: Address, len: usize) -> Result<RegionInfo>
{
    ensure_range_with_permissions(task, addr, len, None)
}

fn ensure_range_with_permissions(task: mach_port_t, addr: Address, len: usize, required: Option<u32>) -> Result<RegionInfo>
{
    if len == 0 {
        return region_for_address(task, addr)?
            .ok_or_else(|| DebuggerError::Io(Error::other("address is not mapped in target task")));
    }

    let info = region_for_address(task, addr)?
        .ok_or_else(|| DebuggerError::Io(Error::other("address is not mapped in target task")))?;

    let start = addr.value();
    let end = start
        .checked_add(len as u64)
        .ok_or_else(|| DebuggerError::Io(Error::other("address range overflowed")))?;
    let region_end = info.start + info.size;

    if end > region_end {
        return Err(DebuggerError::Io(Error::other(
            "requested range crosses a memory region boundary",
        )));
    }

    if let Some(mask) = required {
        if (info.protection & mask) != mask {
            return Err(DebuggerError::Io(Error::new(
                ErrorKind::PermissionDenied,
                "memory range lacks required permissions",
            )));
        }
    }

    Ok(info)
}

fn region_for_address(task: mach_port_t, addr: Address) -> Result<Option<RegionInfo>>
{
    let mut target = addr.value();
    let mut depth: natural_t = 0;

    unsafe {
        loop {
            let mut size: mach_vm_size_t = 0;
            let mut info = vm_region_submap_short_info_data_64_t::default();
            let mut info_count = VM_REGION_SUBMAP_SHORT_INFO_COUNT_64;

            let result = mach_vm_region_recurse(
                task as vm_map_t,
                &mut target,
                &mut size,
                &mut depth,
                &mut info as *mut _ as vm_region_recurse_info_t,
                &mut info_count,
            );

            if result == libc::KERN_INVALID_ADDRESS {
                return Ok(None);
            }
            if result != KERN_SUCCESS {
                return Err(DebuggerError::Io(Error::other(format!(
                    "mach_vm_region_recurse failed: {}",
                    result
                ))));
            }

            if info.is_submap != 0 {
                depth += 1;
                continue;
            }

            return Ok(Some(RegionInfo {
                start: target,
                size,
                protection: info.protection as u32,
                name: region_name_from_user_tag(info.user_tag),
            }));
        }
    }
}

fn protection_to_permissions(protection: c_int) -> String
{
    let mut perms = String::new();
    if (protection & libc::VM_PROT_READ) != 0 {
        perms.push('r');
    }
    if (protection & libc::VM_PROT_WRITE) != 0 {
        perms.push('w');
    }
    if (protection & libc::VM_PROT_EXECUTE) != 0 {
        perms.push('x');
    }
    perms
}

fn region_name_from_user_tag(tag: u32) -> Option<String>
{
    match tag {
        VM_MEMORY_STACK => Some("[stack]".to_string()),
        VM_MEMORY_MALLOC
        | VM_MEMORY_MALLOC_SMALL
        | VM_MEMORY_MALLOC_MEDIUM
        | VM_MEMORY_MALLOC_LARGE
        | VM_MEMORY_MALLOC_HUGE
        | VM_MEMORY_MALLOC_TINY => Some("[heap]".to_string()),
        _ => None,
    }
}

/// Generate region names based on heuristics when user_tag doesn't provide one
///
/// This function uses permission flags to infer likely region types.
/// It's not perfect but provides better UX than empty names.
fn region_name_from_heuristics(protection: c_int) -> Option<String>
{
    use libc::{VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE};

    // Code segments: readable and executable (typically rx or r-x)
    if (protection & VM_PROT_EXECUTE) != 0 && (protection & VM_PROT_READ) != 0 {
        if (protection & VM_PROT_WRITE) == 0 {
            // Read-only executable = code segment
            return Some("[code]".to_string());
        }
        // Writable executable = self-modifying code (rare, but possible)
        return Some("[code (writable)]".to_string());
    }

    // Data segments: readable, writable, but not executable
    if (protection & VM_PROT_READ) != 0 && (protection & VM_PROT_WRITE) != 0 && (protection & VM_PROT_EXECUTE) == 0 {
        // Could be data segment, but we already handle heap via user_tag
        // This might be a data segment or anonymous mapping
        return Some("[data]".to_string());
    }

    // Read-only, non-executable = likely a mapped file or read-only data
    if (protection & VM_PROT_READ) != 0 && (protection & VM_PROT_WRITE) == 0 && (protection & VM_PROT_EXECUTE) == 0 {
        return Some("[rodata]".to_string());
    }

    None
}

fn change_protection(task: mach_port_t, addr: mach_vm_address_t, len: mach_vm_size_t, protection: c_int) -> Result<()>
{
    unsafe {
        let result = mach_vm_protect(task as vm_map_t, addr, len, 1, protection);
        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(Error::other(format!(
                "mach_vm_protect (set maximum) failed: {}",
                result
            ))));
        }

        let result = mach_vm_protect(task as vm_map_t, addr, len, 0, protection);
        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(Error::other(format!("mach_vm_protect failed: {}", result))));
        }
    }

    Ok(())
}

fn aligned_range(addr: Address, len: usize) -> (mach_vm_address_t, mach_vm_size_t)
{
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 };
    let start = addr.value();
    let end = start + len as u64;
    let aligned_start = start & !(page_size - 1);
    let aligned_end = (end + page_size - 1) & !(page_size - 1);
    (aligned_start, aligned_end - aligned_start)
}
