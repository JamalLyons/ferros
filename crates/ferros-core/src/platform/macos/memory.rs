//! # macOS Memory Operations
//!
//! Memory reading and writing using Mach APIs.
//!
//! On macOS, we use `vm_read()` and `vm_write()` to access process memory.
//! These are Mach APIs that work with task ports obtained from `task_for_pid()`.

use libc::{c_int, mach_msg_type_number_t, mach_port_t, vm_address_t, vm_map_t, vm_offset_t, vm_size_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;

use crate::platform::macos::ffi;

/// Constant for mach_vm_region() flavor
/// This is the same constant used by both vm_region (deprecated) and mach_vm_region
const VM_REGION_BASIC_INFO: c_int = 9;

use crate::error::{DebuggerError, Result};
use crate::types::MemoryRegion;

/// Read memory from a Mach task
///
/// Uses `vm_read()` to read memory from the target process. This function allocates memory
/// in the current process's address space, copies the data from the target process, and
/// then returns it as a `Vec<u8>`. The allocated memory is automatically deallocated.
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
pub fn read_memory(task: mach_port_t, addr: u64, len: usize) -> Result<Vec<u8>>
{
    unsafe {
        let mut data: vm_offset_t = 0;
        let mut data_count: mach_msg_type_number_t = 0;

        let result = ffi::vm_read(
            task as vm_map_t,
            addr as vm_address_t,
            len as vm_size_t,
            &mut data,
            &mut data_count,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("vm_read() failed with error code: {}", result),
            )));
        }

        // Copy the data into a Vec<u8>
        // vm_read() allocates memory in our address space, so we need to copy it
        // to a Vec before deallocating the Mach-allocated memory
        let bytes_read = data_count as usize;
        let mut buffer = vec![0u8; bytes_read];
        std::ptr::copy_nonoverlapping(data as *const u8, buffer.as_mut_ptr(), bytes_read);

        // Deallocate the memory allocated by vm_read()
        // vm_read() allocates memory that we must free
        ffi::vm_deallocate(task as vm_map_t, data as vm_address_t, data_count as vm_size_t);

        Ok(buffer)
    }
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
pub fn write_memory(task: mach_port_t, addr: u64, data: &[u8]) -> Result<usize>
{
    unsafe {
        let result = ffi::vm_write(
            task as vm_map_t,
            addr as vm_address_t,
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
    let mut address: u64 = 0;

    unsafe {
        loop {
            let mut size: u64 = 0;
            let mut info: ffi::VmRegionBasicInfoData = ffi::VmRegionBasicInfoData {
                protection: 0,
                max_protection: 0,
                inheritance: 0,
                shared: 0,
                reserved: 0,
                offset: 0,
                behavior: 0,
                user_wired_count: 0,
            };
            let mut info_count: mach_msg_type_number_t =
                std::mem::size_of::<ffi::VmRegionBasicInfoData>() as mach_msg_type_number_t;
            let mut object_name: mach_port_t = 0;

            let result = ffi::mach_vm_region(
                task as vm_map_t,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO,
                &mut info,
                &mut info_count,
                &mut object_name,
            );

            if result != KERN_SUCCESS {
                break;
            }

            // Convert protection flags to permission string
            // VM_PROT_READ, VM_PROT_WRITE, and VM_PROT_EXECUTE are bit flags
            // that can be combined (e.g., rwx, r-x, rw-)
            let mut perms = String::new();
            if (info.protection & libc::VM_PROT_READ as u32) != 0 {
                perms.push('r');
            }
            if (info.protection & libc::VM_PROT_WRITE as u32) != 0 {
                perms.push('w');
            }
            if (info.protection & libc::VM_PROT_EXECUTE as u32) != 0 {
                perms.push('x');
            }

            // Create a MemoryRegion from the information we gathered
            // Note: macOS's mach_vm_region doesn't easily provide region names
            // (like "[heap]" or "[stack]"), so we leave it as None
            regions.push(MemoryRegion::new(
                address as u64,
                (address + size) as u64,
                perms,
                None, // macOS mach_vm_region doesn't provide names easily
            ));

            // Move to the next memory region
            // The next region starts right after this one ends
            address += size;
        }
    }

    Ok(regions)
}
