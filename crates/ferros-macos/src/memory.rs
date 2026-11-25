//! # macOS Memory Operations
//!
//! Memory reading and writing using Mach APIs.
//!
//! On macOS, we use `vm_read()` and `vm_write()` to access process memory.
//! These are Mach APIs that work with task ports obtained from `task_for_pid()`.
//!
//! ## Mach Memory APIs
//!
//! - **vm_read()**: Read memory from a target process
//! - **vm_write()**: Write memory to a target process
//! - **mach_vm_region()**: Enumerate memory regions in a process
//! - **mach_vm_protect()**: Change memory protection flags
//!
//! ## References
//!
//! - [vm_read(3) man page](https://developer.apple.com/documentation/kernel/1402149-vm_read/)
//! - [vm_write(3) man page](https://developer.apple.com/documentation/kernel/1402149-vm_write/)
//! - [mach_vm_region(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_region/)
//! - [mach_vm_protect(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_protect/)

use std::cmp::min;
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::{Arc, RwLock};

use ferros_core::BackendError;
use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
use libc::{c_int, mach_port_t, vm_address_t, vm_map_t, vm_offset_t};
#[cfg(target_os = "macos")]
use mach2::kern_return::KERN_SUCCESS;
use mach2::message::mach_msg_type_number_t;
#[cfg(target_os = "macos")]
use mach2::vm::{mach_vm_machine_attribute, mach_vm_protect, mach_vm_read_overwrite, mach_vm_region_recurse};
#[cfg(target_os = "macos")]
use mach2::vm_attributes::{MATTR_CACHE, MATTR_VAL_CACHE_FLUSH, vm_machine_attribute_val_t};
#[cfg(target_os = "macos")]
use mach2::vm_region::{
    VM_REGION_SUBMAP_SHORT_INFO_COUNT_64, vm_region_recurse_info_t, vm_region_submap_short_info_data_64_t,
};
#[cfg(target_os = "macos")]
use mach2::vm_statistics::{
    VM_MEMORY_MALLOC, VM_MEMORY_MALLOC_HUGE, VM_MEMORY_MALLOC_LARGE, VM_MEMORY_MALLOC_MEDIUM, VM_MEMORY_MALLOC_SMALL,
    VM_MEMORY_MALLOC_TINY, VM_MEMORY_STACK,
};
#[cfg(target_os = "macos")]
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t, natural_t};
use once_cell::sync::Lazy;

use crate::error::{MachError, vm_read_error};
use crate::{constants, ffi};

// Use constants from the centralized constants module

static SYSTEM_PAGE_SIZE: Lazy<usize> = Lazy::new(|| unsafe {
    let size = libc::sysconf(libc::_SC_PAGESIZE);
    if size <= 0 { 4096 } else { size as usize }
});

fn page_align_down(value: u64, page_size: usize) -> u64
{
    let mask = !(page_size as u64 - 1);
    value & mask
}

/// Simple read-through memory cache that stores pages fetched from the target.
pub struct MemoryCache
{
    page_size: usize,
    pages: RwLock<HashMap<u64, Arc<Vec<u8>>>>,
}

impl Default for MemoryCache
{
    fn default() -> Self
    {
        Self::with_page_size(*SYSTEM_PAGE_SIZE)
    }
}

impl MemoryCache
{
    /// Create a cache using the system page size.
    pub fn new() -> Self
    {
        Self::default()
    }

    /// Create a cache with a custom page size (must be power of two).
    pub fn with_page_size(page_size: usize) -> Self
    {
        let size = page_size.max(1024).next_power_of_two();
        Self {
            page_size: size,
            pages: RwLock::new(HashMap::new()),
        }
    }

    /// Clears all cached pages.
    pub fn clear(&self)
    {
        self.pages.write().unwrap().clear();
    }

    /// Invalidates any cached pages overlapping the provided range.
    pub fn invalidate_range(&self, addr: Address, len: usize)
    {
        if len == 0 {
            return;
        }
        let start = addr.value();
        let end = start.saturating_add(len as u64);
        let mut pages = self.pages.write().unwrap();
        let page_size = self.page_size as u64;
        let mut base = page_align_down(start, self.page_size);
        while base < end {
            pages.remove(&base);
            base = base.saturating_add(page_size);
        }
    }

    fn fetch_page(&self, task: mach_port_t, base: u64) -> Result<Arc<Vec<u8>>>
    {
        if let Some(existing) = self.pages.read().unwrap().get(&base) {
            return Ok(existing.clone());
        }

        let mut pages = self.pages.write().unwrap();
        if let Some(existing) = pages.get(&base) {
            return Ok(existing.clone());
        }

        let data = read_memory(task, Address::from(base), self.page_size)?;
        let arc = Arc::new(data);
        pages.insert(base, arc.clone());
        Ok(arc)
    }

    /// Reads a range, using cached pages when available.
    ///
    /// A cache page can be larger than a VM page (see [`with_page_size`](Self::with_page_size)),
    /// so the cache page around readable bytes can include an unmapped or unreadable VM
    /// page that the requested range does not touch. When a cache page cannot be read,
    /// the part of the range in it is read exactly instead, and nothing is cached for it;
    /// that exact read's error is the one reported.
    ///
    /// # Errors
    /// The errors of [`read_memory`] for the bytes of the range itself.
    pub fn read(&self, task: mach_port_t, addr: Address, len: usize) -> Result<Vec<u8>>
    {
        if len == 0 {
            return Ok(Vec::new());
        }
        range_end(addr, len)?;

        // Grown chunk by chunk rather than allocated up front: a length no mapping can
        // hold (`usize::MAX`) then fails at its first unreadable page without trying to
        // allocate it.
        let mut output = Vec::new();
        while output.len() < len {
            let absolute = addr.value() + output.len() as u64;
            let page_base = page_align_down(absolute, self.page_size);
            let page_offset = (absolute - page_base) as usize;
            // Never zero: `page_offset < page_size` and `output.len() < len`.
            let chunk = (len - output.len()).min(self.page_size - page_offset);
            match self.fetch_page(task, page_base) {
                Ok(page) if page_offset + chunk <= page.len() => {
                    output.extend_from_slice(&page[page_offset..page_offset + chunk]);
                }
                // `read_memory` is all or nothing, so a page is never short; an exact read
                // of the chunk covers that case and a page that could not be read whole.
                Ok(_) | Err(_) => output.extend(read_memory(task, Address::from(absolute), chunk)?),
            }
        }

        Ok(output)
    }

    /// Reads a 64-bit little-endian value.
    pub fn read_u64(&self, task: mach_port_t, addr: Address) -> Result<u64>
    {
        let bytes = self.read(task, addr, 8)?;
        if bytes.len() < 8 {
            return Err(DebuggerError::Io(Error::new(
                ErrorKind::UnexpectedEof,
                "failed to read full 8 bytes",
            )));
        }
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

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
/// A `Vec<u8>` of exactly `len` bytes. The read is all or nothing.
///
/// ## Errors
///
/// The error wraps a [`BackendError`](ferros_core::BackendError), classified while the
/// kernel's answer is still known:
/// - `NotFound` if any byte of the range is unmapped
/// - `PermissionDenied` (as [`DebuggerError::PermissionDenied`]) if a byte is mapped but
///   not readable
/// - `Os` with the native code and the address for any other failure
///
/// ## No `Io` compatibility adapter
///
/// Before issue #216 these failures were `DebuggerError::Io` with an
/// [`ErrorKind`](std::io::ErrorKind). They are not mapped back to `Io` for older callers,
/// and no adapter that does so is provided: the `Backend` boundary turned an `Io` read
/// error into `Os { code: -1 }`, losing the category every caller needs, and every caller
/// in the workspace matches the typed errors above. The crate is unpublished (`0.0.0`),
/// so there are no outside callers to keep. The same holds for [`read_memory_into`],
/// [`MemoryCache::read`] and `diagnostics::raw_memory`.
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
    // Every byte is checked before the buffer is allocated, so a length no mapping can
    // hold (`usize::MAX`, or one running past the end of the address space) fails as
    // `NotFound` without trying to allocate it.
    ensure_readable_span(task, addr, len)?;

    let mut buffer = vec![0u8; len];
    read_checked_span(addr, &mut buffer, |cursor, dst| read_overwrite(task, cursor, dst))?;
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
/// ## Instruction cache
///
/// `vm_write()` changes memory, not the instruction caches of the cores the target runs
/// on. A thread that has the old instruction cached keeps executing it: it runs past a
/// new `brk`, or traps on a `brk` that has been replaced (issue #80). After every write
/// the written range is flushed with `MATTR_CACHE`, as LLDB's debugserver does, so the
/// next instruction any thread fetches there is the one in memory.
///
/// See: [vm_write(3) man page](https://developer.apple.com/documentation/kernel/1585462-vm_write/)
pub fn write_memory(task: mach_port_t, addr: Address, data: &[u8]) -> Result<usize>
{
    if data.is_empty() {
        return Ok(0);
    }
    let Ok(count) = mach_msg_type_number_t::try_from(data.len()) else {
        return Err(DebuggerError::InvalidArgument(format!(
            "a write of {} bytes at 0x{:016x} is larger than vm_write accepts",
            data.len(),
            addr.value()
        )));
    };

    ensure_range(task, addr, data.len())?;

    unsafe {
        let result = ffi::vm_write(
            task as vm_map_t,
            addr.value() as vm_address_t,
            data.as_ptr() as vm_offset_t,
            count,
        );

        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("vm_write() failed with error code: {}", result),
            )));
        }
    }

    flush_caches(task, addr, data.len())?;
    Ok(data.len())
}

/// Write `data` at `addr`, all or nothing, making a read-only page writable if needed.
///
/// A direct [`write_memory`] is tried first. Code pages are not writable, so if it fails
/// the range is made writable with a [`MemoryProtectionGuard`], written, and the guard
/// restores the original protection.
///
/// ## All or nothing
///
/// This is what `Backend::write_memory` promises the engine, which rolls back its own
/// multi-site trap updates assuming a failed write changed nothing (issue #133). Most
/// failures happen before any byte is written: an unmapped range, a range that crosses
/// a region boundary (checked first), or a refused protection change. But `vm_write`
/// can succeed and the cache flush after it fail, which leaves memory changed. So the
/// bytes in the range are read before the write; if the write fails and memory no
/// longer holds them, they are written back the same way and the original error is
/// returned.
///
/// The guarantee has two limits, both reported rather than hidden: a range that cannot
/// be read beforehand gets no rollback, and if the rollback itself fails the error says
/// that memory may be partly written.
///
/// ## Protection restore
///
/// A write that had to make the range writable fails if the original protection cannot
/// be put back afterwards (issue #139), even though the bytes were written: a code page
/// left `rw-` faults the moment the target executes it. The written bytes are then
/// rolled back as for any other failed write, and the error names the address and the
/// protection that could not be restored, because the range stays writable and not
/// executable.
pub fn write_memory_all_or_nothing(task: mach_port_t, addr: Address, data: &[u8]) -> Result<()>
{
    write_restoring(task, addr, data, write_with_protection)
}

/// [`write_memory`], retried with the range made writable when the direct write fails.
fn write_with_protection(task: mach_port_t, addr: Address, data: &[u8]) -> Result<()>
{
    write_with_protection_using(task, addr, data, change_protection)
}

/// [`write_with_protection`], with `restore` putting back the original protection (a
/// seam so tests can make the restore fail).
fn write_with_protection_using(
    task: mach_port_t,
    addr: Address,
    data: &[u8],
    restore: impl FnMut(mach_port_t, mach_vm_address_t, mach_vm_size_t, c_int) -> Result<()>,
) -> Result<()>
{
    match write_memory(task, addr, data) {
        Ok(_) => Ok(()),
        Err(first) => {
            tracing::debug!(
                "Direct write at 0x{:016x} failed ({first}); retrying with the page made writable",
                addr.value()
            );
            let guard = MemoryProtectionGuard::make_writable(task, addr, data.len())?;
            let written = write_memory(task, addr, data);
            match (written, guard.restore_with(restore)) {
                (Ok(_), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                (Err(error), Err(restore)) => Err(DebuggerError::Io(Error::other(format!("{error}; {restore}")))),
            }
        }
    }
}

/// Run `write`, and if it fails, put back the bytes the range held before it (see
/// [`write_memory_all_or_nothing`]).
fn write_restoring(
    task: mach_port_t,
    addr: Address,
    data: &[u8],
    mut write: impl FnMut(mach_port_t, Address, &[u8]) -> Result<()>,
) -> Result<()>
{
    let before = read_memory(task, addr, data.len())
        .ok()
        .filter(|before| before.len() == data.len());
    let Err(error) = write(task, addr, data) else {
        return Ok(());
    };
    let Some(before) = before else {
        return Err(error);
    };
    if read_memory(task, addr, before.len()).is_ok_and(|now| now == before) {
        return Err(error);
    }
    match write(task, addr, &before) {
        Ok(()) => {
            tracing::warn!(
                "a failed write at 0x{:016x} had changed memory; the original bytes were restored",
                addr.value()
            );
            Err(error)
        }
        // The restore can put the bytes back and still fail, as when the cache flush is
        // what failed the first write too: memory then holds its original bytes.
        Err(rollback) if read_memory(task, addr, before.len()).is_ok_and(|now| now == before) => {
            tracing::warn!(
                "a failed write at 0x{:016x} had changed memory; the original bytes are back, but restoring them reported: \
                 {rollback}",
                addr.value()
            );
            Err(error)
        }
        Err(rollback) => Err(DebuggerError::Io(Error::other(format!(
            "{error}; restoring the original {} bytes at 0x{:016x} also failed ({rollback}), so that memory may be partly \
             written",
            before.len(),
            addr.value()
        )))),
    }
}

/// Write back data and invalidate instructions cached for `len` bytes at `addr`.
///
/// XNU syncs a page this way when it first maps it executable, never after a later
/// write. The kernel flushes one page per call, so the range is split at page boundaries.
///
/// See: [vm_machine_attribute(3)](https://developer.apple.com/documentation/kernel/1402219-vm_machine_attribute/)
fn flush_caches(task: mach_port_t, addr: Address, len: usize) -> Result<()>
{
    let page_size = *SYSTEM_PAGE_SIZE as u64;
    let mut start = addr.value();
    let end = start
        .checked_add(len as u64)
        .ok_or_else(|| DebuggerError::InvalidArgument(format!("range at 0x{start:016x} overflows")))?;
    while start < end {
        let page_end = page_align_down(start, *SYSTEM_PAGE_SIZE).saturating_add(page_size);
        let size = page_end.min(end) - start;
        let mut value: vm_machine_attribute_val_t = MATTR_VAL_CACHE_FLUSH;
        // SAFETY: `value` is a valid, writable attribute value; the kernel only reads
        // and writes that one value and validates the range in the target map itself.
        let result = unsafe { mach_vm_machine_attribute(task, start, size, MATTR_CACHE, &raw mut value) };
        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(Error::other(format!(
                "flushing the caches for 0x{start:016x}+{size} failed with error code: {result}"
            ))));
        }
        start += size;
    }
    Ok(())
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
///
/// All or nothing, like [`read_memory`], with the same errors; returns `dst.len()`.
pub fn read_memory_into(task: mach_port_t, addr: Address, dst: &mut [u8]) -> Result<usize>
{
    if dst.is_empty() {
        return Ok(0);
    }

    ensure_readable_span(task, addr, dst.len())?;
    read_checked_span(addr, dst, |cursor, dst| read_overwrite(task, cursor, dst))
}

/// One `mach_vm_read_overwrite` of `dst.len()` bytes at `cursor` into `dst`: the kernel's
/// result and the byte count it reports.
fn read_overwrite(task: mach_port_t, cursor: u64, dst: &mut [u8]) -> (libc::kern_return_t, mach_vm_size_t)
{
    let mut actual: mach_vm_size_t = 0;
    // SAFETY: `dst` is valid for `dst.len()` writable bytes in this process, which is all
    // the kernel writes; `actual` is a valid out-parameter.
    let result = unsafe {
        mach_vm_read_overwrite(
            task as vm_map_t,
            cursor,
            dst.len() as mach_vm_size_t,
            dst.as_mut_ptr() as mach_vm_address_t,
            &mut actual,
        )
    };
    (result, actual)
}

/// Fill `dst` from `addr` in chunks of at most [`constants::MAX_VM_READ_CHUNK`] bytes,
/// after the caller checked that every byte is mapped and readable.
///
/// `read_chunk` is one `mach_vm_read_overwrite` (a seam so tests can make the kernel
/// answer what only a map changing under the read produces). A failed chunk is
/// classified by [`vm_read_error`]; a chunk that reads zero bytes, or more than asked,
/// is `NotFound` rather than a short buffer or an endless loop. A short chunk is
/// continued from where it stopped.
fn read_checked_span(
    addr: Address,
    dst: &mut [u8],
    mut read_chunk: impl FnMut(u64, &mut [u8]) -> (libc::kern_return_t, mach_vm_size_t),
) -> Result<usize>
{
    let mut total = 0usize;
    let mut cursor = addr.value();

    while total < dst.len() {
        let chunk_len = min(constants::MAX_VM_READ_CHUNK, dst.len() - total);
        let (result, actual) = read_chunk(cursor, &mut dst[total..total + chunk_len]);

        if result != KERN_SUCCESS {
            return Err(vm_read_error("mach_vm_read_overwrite", Address::from(cursor), result).into());
        }

        // Every byte was checked readable above, so the kernel stopping short means the
        // map changed under the read. Report it rather than return a short buffer (or,
        // for zero bytes, loop forever).
        if actual == 0 || actual > chunk_len as mach_vm_size_t {
            return Err(BackendError::NotFound(format!(
                "mach_vm_read_overwrite read {} of {} bytes at {}",
                total as u64 + actual,
                dst.len(),
                addr
            ))
            .into());
        }

        total += actual as usize;
        cursor += actual;
    }

    Ok(total)
}

/// Formats bytes into a traditional hex + ASCII view.
pub fn format_hexdump(base: Address, bytes: &[u8], width: usize) -> String
{
    let width = width.clamp(8, 32);
    let mut out = String::new();
    for (offset, chunk) in bytes.chunks(width).enumerate() {
        let addr = base.value().saturating_add((offset * width) as u64);
        out.push_str(&format!("{addr:016x}: "));

        for i in 0..width {
            if i < chunk.len() {
                out.push_str(&format!("{:02x} ", chunk[i]));
            } else {
                out.push_str("   ");
            }
        }

        out.push(' ');
        for byte in chunk {
            let ch = if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            };
            out.push(ch);
        }
        out.push('\n');
    }
    out
}

/// Read and format memory as a hexdump string.
///
/// Convenience function that reads memory and formats it using `format_hexdump()`.
/// Uses `read_memory()` internally.
///
/// See: [vm_read(3) man page](https://developer.apple.com/documentation/kernel/1585350-vm_read/)
pub fn hexdump_memory(
    task: mach_port_t,
    cache: Option<&MemoryCache>,
    addr: Address,
    len: usize,
    width: usize,
) -> Result<String>
{
    let bytes = if let Some(cache) = cache {
        cache.read(task, addr, len)?
    } else {
        read_memory(task, addr, len)?
    };
    Ok(format_hexdump(addr, &bytes, width))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize>
{
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Search for a byte pattern in memory.
///
/// Scans memory for a specific byte pattern, similar to `grep` for memory.
/// Can use a `MemoryCache` for faster repeated searches.
///
/// ## Parameters
///
/// - `task`: Mach task port
/// - `cache`: Optional memory cache (speeds up repeated searches)
/// - `start`: Starting address to search from
/// - `len`: Number of bytes to search
/// - `pattern`: Byte pattern to search for
///
/// ## Returns
///
/// `Some(address)` if the pattern is found, `None` otherwise.
///
/// ## Implementation
///
/// Reads memory in chunks and searches for the pattern, handling boundary
/// cases where the pattern might span chunk boundaries.
pub fn find_pattern(
    task: mach_port_t,
    cache: Option<&MemoryCache>,
    start: Address,
    len: usize,
    pattern: &[u8],
) -> Result<Option<Address>>
{
    if pattern.is_empty() {
        return Ok(Some(start));
    }

    let mut scanned = 0usize;
    while scanned < len {
        let chunk_len = min(constants::PATTERN_SCAN_CHUNK, len - scanned);
        let addr = Address::from(start.value().saturating_add(scanned as u64));
        let chunk = if let Some(cache) = cache {
            cache.read(task, addr, chunk_len)?
        } else {
            read_memory(task, addr, chunk_len)?
        };

        if let Some(pos) = find_subslice(&chunk, pattern) {
            let absolute = start.value().saturating_add(scanned as u64 + pos as u64);
            return Ok(Some(Address::from(absolute)));
        }

        if chunk_len < pattern.len() {
            break;
        }

        // Overlap by pattern length to catch boundary matches.
        let step = chunk_len.saturating_sub(pattern.len().saturating_sub(1));
        if step == 0 {
            break;
        }
        scanned += step;
    }

    Ok(None)
}

/// `VM_PROT_COPY` from `<mach/vm_prot.h>` (not exported by the `libc` crate): make a
/// private copy of the pages so that protection beyond the maximum can be set.
const VM_PROT_COPY: c_int = 0x10;

/// Guard that temporarily changes the protection of a memory range and restores it on drop.
///
/// This RAII guard uses `mach_vm_protect()` to temporarily change memory protection
/// (e.g., make read-only memory writable) and automatically restores the original
/// protection when dropped.
///
/// ## Use Cases
///
/// - Installing software breakpoints (need to write INT3/BRK to code segments)
/// - Temporarily making memory writable for patching
///
/// ## Safety
///
/// The guard ensures memory protection is restored even if an error occurs,
/// preventing memory corruption or security issues.
///
/// See: [mach_vm_protect(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_protect/)
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
    /// Make the specified range writable until the guard is dropped.
    ///
    /// Sets protection to `VM_PROT_READ | VM_PROT_WRITE` when the region's maximum
    /// protection allows writes (the range is not executable until the guard restores
    /// the original protection). Otherwise (code pages, whose maximum
    /// protection is `r-x`) it requests `VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY`:
    /// the kernel gives the target a private copy-on-write copy of the page that may be
    /// written, as debuggers do to plant breakpoints. Dropping the guard restores the
    /// original protection.
    ///
    /// See: [mach_vm_protect(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_protect/)
    pub fn make_writable(task: mach_port_t, addr: Address, len: usize) -> Result<Self>
    {
        let max_allows_write = len == 0 || (ensure_range(task, addr, len)?.max_protection & libc::VM_PROT_WRITE as u32) != 0;
        let protection = if max_allows_write {
            libc::VM_PROT_READ | libc::VM_PROT_WRITE
        } else {
            libc::VM_PROT_READ | libc::VM_PROT_WRITE | VM_PROT_COPY
        };
        Self::with_protection(task, addr, len, protection)
    }

    /// Apply an arbitrary protection mask for the lifetime of the guard.
    ///
    /// Allows setting custom protection flags (e.g., `VM_PROT_READ | VM_PROT_EXECUTE`).
    /// Write access beyond the region's maximum protection is only possible together
    /// with `VM_PROT_COPY`.
    ///
    /// See: [mach_vm_protect(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_protect/)
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

        // Check if the requested protection is allowed by the region's maximum protection
        let requested_write = (protection & libc::VM_PROT_WRITE) != 0;
        let max_allows_write = (region.max_protection & libc::VM_PROT_WRITE as u32) != 0;

        let copy_on_write = (protection & VM_PROT_COPY) != 0;

        if requested_write && !max_allows_write && !copy_on_write {
            return Err(DebuggerError::Io(Error::other(format!(
                "Cannot make memory writable at 0x{:016x}: region maximum protection (0x{:x}) does not allow writes. \
                 Maximum protection cannot be changed. Consider using hardware breakpoints instead.",
                addr.value(),
                region.max_protection
            ))));
        }

        change_protection(task, aligned_addr, aligned_len, protection)?;

        Ok(Self {
            task,
            addr: aligned_addr,
            len: aligned_len,
            original,
            active: true,
        })
    }

    /// Put back the original protection, reporting a failure.
    ///
    /// A refused change is retried once. Dropping the guard also restores the
    /// protection, but can only log a failure; call this wherever the outcome matters,
    /// as after writing code (issue #139).
    pub fn restore(self) -> Result<()>
    {
        self.restore_with(change_protection)
    }

    fn restore_with(
        mut self,
        mut protect: impl FnMut(mach_port_t, mach_vm_address_t, mach_vm_size_t, c_int) -> Result<()>,
    ) -> Result<()>
    {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let Err(first) = protect(self.task, self.addr, self.len, self.original) else {
            return Ok(());
        };
        tracing::debug!(
            "restoring the protection of 0x{:016x}+{} failed ({first}); retrying once",
            self.addr,
            self.len
        );
        protect(self.task, self.addr, self.len, self.original).map_err(|error| self.restore_error(&error))
    }

    fn restore_error(&self, error: &DebuggerError) -> DebuggerError
    {
        DebuggerError::Io(Error::other(format!(
            "the protection of 0x{:016x}+{} could not be restored to {} ({error}), so the range is left writable",
            self.addr,
            self.len,
            protection_label(self.original)
        )))
    }
}

impl Drop for MemoryProtectionGuard
{
    fn drop(&mut self)
    {
        if self.active
            && let Err(error) = change_protection(self.task, self.addr, self.len, self.original)
        {
            tracing::warn!("{}", self.restore_error(&error));
        }
    }
}

/// `protection` as `rwx` flags, with `-` for a missing one.
fn protection_label(protection: c_int) -> String
{
    [
        (libc::VM_PROT_READ, 'r'),
        (libc::VM_PROT_WRITE, 'w'),
        (libc::VM_PROT_EXECUTE, 'x'),
    ]
    .iter()
    .map(|&(flag, letter)| if protection & flag != 0 { letter } else { '-' })
    .collect()
}

#[derive(Debug, Clone)]
struct RegionInfo
{
    start: mach_vm_address_t,
    size: mach_vm_size_t,
    protection: u32,
    max_protection: u32,
}

/// The end of `[addr, addr + len)`, or `NotFound` when it runs past the end of the
/// address space, where no byte can be mapped.
fn range_end(addr: Address, len: usize) -> Result<u64>
{
    addr.value().checked_add(len as u64).ok_or_else(|| {
        DebuggerError::from(BackendError::NotFound(format!(
            "range of {len} bytes at {addr} runs past the end of the address space"
        )))
    })
}

/// Check that every byte of `[addr, addr + len)` is mapped and readable.
///
/// The range may span several adjacent regions. A hole is `NotFound` and an unreadable
/// region is `PermissionDenied`, whichever part of the range it falls in, so a read that
/// runs off the end of a mapping fails the same way as one that starts outside it.
fn ensure_readable_span(task: mach_port_t, addr: Address, len: usize) -> Result<()>
{
    let end = range_end(addr, len)?;
    let mut cursor = addr.value();
    while cursor < end {
        let at = Address::from(cursor);
        let info = region_for_address(task, at)?
            .ok_or_else(|| DebuggerError::from(BackendError::NotFound(format!("address {at} is not mapped"))))?;
        if (info.protection & libc::VM_PROT_READ as u32) == 0 {
            return Err(BackendError::PermissionDenied(format!("memory at {at} is not readable")).into());
        }
        let next = info.start.saturating_add(info.size);
        if next <= cursor {
            return Err(BackendError::NotFound(format!("address {at} is not mapped")).into());
        }
        cursor = next;
    }
    Ok(())
}

fn ensure_range(task: mach_port_t, addr: Address, len: usize) -> Result<RegionInfo>
{
    ensure_range_with_permissions(task, addr, len, None)
}

fn ensure_range_with_permissions(task: mach_port_t, addr: Address, len: usize, required: Option<u32>) -> Result<RegionInfo>
{
    if len == 0 {
        return region_for_address(task, addr)?
            .ok_or_else(|| DebuggerError::Io(Error::new(ErrorKind::NotFound, "address is not mapped in target task")));
    }

    let info = region_for_address(task, addr)?
        .ok_or_else(|| DebuggerError::Io(Error::new(ErrorKind::NotFound, "address is not mapped in target task")))?;

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

    if let Some(mask) = required
        && (info.protection & mask) != mask
    {
        return Err(DebuggerError::Io(Error::new(
            ErrorKind::PermissionDenied,
            "memory range lacks required permissions",
        )));
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
                return Err(BackendError::Os {
                    code: i64::from(result),
                    message: format!("mach_vm_region_recurse at {addr}: {}", MachError::from(result)),
                }
                .into());
            }

            if target > addr.value() {
                return Ok(None);
            }
            if info.is_submap != 0 {
                depth += 1;
                // The kernel replaced target with the enclosing submap's start.
                // Descend at the original query, not the first region in that submap.
                target = addr.value();
                continue;
            }

            return Ok(Some(RegionInfo {
                start: target,
                size,
                protection: info.protection as u32,
                max_protection: info.max_protection as u32,
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
        // On macOS, we can only set current protection to what the maximum allows.
        // We don't try to change maximum protection (set_maximum=1) because:
        // 1. Maximum protection is set by the kernel and typically can't be changed
        // 2. Code segments often have max_protection = READ|EXECUTE (no write)
        // 3. If max doesn't allow writes, we can't make it writable

        // Just set current protection (set_maximum=0)
        // This will fail if the requested protection exceeds the maximum, which is expected
        let result = mach_vm_protect(task as vm_map_t, addr, len, 0, protection);
        if result != KERN_SUCCESS {
            return Err(DebuggerError::Io(Error::other(format!(
                "mach_vm_protect failed: {} - requested protection may exceed region maximum",
                result
            ))));
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

#[cfg(all(test, target_os = "macos"))]
mod tests
{
    use std::cell::{Cell, UnsafeCell};

    use mach2::traps::mach_task_self;

    use super::*;

    fn task() -> mach_port_t
    {
        // SAFETY: returns this process's own task port; no resource is acquired.
        unsafe { mach_task_self() }
    }

    /// Bytes of this process that the kernel writes through its task port.
    struct Target(UnsafeCell<[u8; 4]>);

    impl Target
    {
        fn new() -> Self
        {
            Self(UnsafeCell::new([1, 2, 3, 4]))
        }

        fn address(&self) -> Address
        {
            Address::from(self.0.get() as u64)
        }

        fn bytes(&self) -> [u8; 4]
        {
            // SAFETY: the pointer is valid and aligned; no reference to the bytes exists
            // while `vm_write` changes them behind Rust's back.
            unsafe { self.0.get().read_volatile() }
        }
    }

    fn failure() -> DebuggerError
    {
        DebuggerError::Io(Error::other("injected failure"))
    }

    #[test]
    fn write_that_changed_memory_before_failing_is_rolled_back()
    {
        // Issue #133: `vm_write` succeeded and the cache flush after it failed.
        let target = Target::new();
        let addr = target.address();
        let calls = Cell::new(0);
        let result = write_restoring(task(), addr, &[9; 4], |task, addr, data| {
            calls.set(calls.get() + 1);
            write_memory(task, addr, data)?;
            if calls.get() == 1 { Err(failure()) } else { Ok(()) }
        });
        assert!(result.unwrap_err().to_string().contains("injected failure"));
        assert_eq!(calls.get(), 2);
        assert_eq!(target.bytes(), [1, 2, 3, 4]);
    }

    #[test]
    fn failed_write_that_changed_nothing_is_not_written_again()
    {
        let target = Target::new();
        let addr = target.address();
        let calls = Cell::new(0);
        let result = write_restoring(task(), addr, &[9; 4], |_, _, _| {
            calls.set(calls.get() + 1);
            Err(failure())
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
        assert_eq!(target.bytes(), [1, 2, 3, 4]);
    }

    #[test]
    fn failed_rollback_reports_memory_may_be_partly_written()
    {
        let target = Target::new();
        let addr = target.address();
        let calls = Cell::new(0);
        let result = write_restoring(task(), addr, &[9; 4], |task, addr, data| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                write_memory(task, addr, data)?;
            }
            Err(failure())
        });
        let message = result.unwrap_err().to_string();
        assert!(message.contains("injected failure"), "{message}");
        assert!(message.contains("partly written"), "{message}");
        assert_eq!(target.bytes(), [9; 4]);
    }

    #[test]
    fn rollback_that_restored_the_bytes_but_failed_reports_only_the_write_error()
    {
        // Both the write and its restore change memory and then fail, like a cache flush
        // failing after each `vm_write`.
        let target = Target::new();
        let addr = target.address();
        let result = write_restoring(task(), addr, &[9; 4], |task, addr, data| {
            write_memory(task, addr, data)?;
            Err(failure())
        });
        let message = result.unwrap_err().to_string();
        assert!(message.contains("injected failure"), "{message}");
        assert!(!message.contains("partly written"), "{message}");
        assert_eq!(target.bytes(), [1, 2, 3, 4]);
    }

    /// A read-only page of this process, so a direct `vm_write` is refused and the write
    /// goes through [`MemoryProtectionGuard`] like a write to code does.
    struct ReadOnlyPage
    {
        base: *mut libc::c_void,
        len: usize,
    }

    impl ReadOnlyPage
    {
        fn new() -> Self
        {
            let len = *SYSTEM_PAGE_SIZE;
            // SAFETY: a fresh anonymous private mapping; nothing else refers to it.
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANON | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            assert_ne!(base, libc::MAP_FAILED);
            // SAFETY: the mapping is `len` writable bytes and owned by this value.
            unsafe {
                base.cast::<[u8; 4]>().write_volatile([1, 2, 3, 4]);
                assert_eq!(libc::mprotect(base, len, libc::PROT_READ), 0);
            }
            Self { base, len }
        }

        fn address(&self) -> Address
        {
            Address::from(self.base as u64)
        }

        fn bytes(&self) -> [u8; 4]
        {
            // SAFETY: the mapping stays readable for the lifetime of `self`.
            unsafe { self.base.cast::<[u8; 4]>().read_volatile() }
        }

        fn protection(&self) -> u32
        {
            region_for_address(task(), self.address()).unwrap().unwrap().protection
        }
    }

    impl Drop for ReadOnlyPage
    {
        fn drop(&mut self)
        {
            // SAFETY: unmaps the mapping this value created and nothing else uses.
            unsafe { libc::munmap(self.base, self.len) };
        }
    }

    // Issue #139: a failed protection restore must fail the write, not leave a code page
    // writable and non-executable behind an `Ok`.
    //
    // These tests inject the restore failure because the real backend cannot be made to
    // produce one, and the same goes for a write that changes memory and then fails
    // (issue #133's second criterion), so the rollback tests above inject that failure
    // too. Concretely:
    // - The restore asks `mach_vm_protect` for the protection the region had a moment
    //   earlier, which never exceeds its maximum, so the kernel refuses it only if the
    //   mapping changed in between (unmapped or remapped). The target is suspended while
    //   the debugger writes, so nothing can change it, and racing an unmap from another
    //   thread in this process would make a nondeterministic test.
    // - `vm_write` is checked against one region before it runs (`ensure_range`), and
    //   the `MATTR_CACHE` flush after it fails only on a range that is no longer mapped,
    //   which again needs the mapping to change between two calls.
    // A real-process test in `ferros-tests` would need the same injection reachable from
    // outside this crate, which is instrumentation the production path should not carry.
    #[test]
    fn failed_protection_restore_fails_the_write_and_rolls_it_back()
    {
        let page = ReadOnlyPage::new();
        let addr = page.address();
        let restores = Cell::new(0);
        let result = write_restoring(task(), addr, &[9; 4], |task, addr, data| {
            write_with_protection_using(task, addr, data, |_, _, _, _| {
                restores.set(restores.get() + 1);
                Err(failure())
            })
        });
        let message = result.unwrap_err().to_string();
        assert!(message.contains("could not be restored to r--"), "{message}");
        assert!(message.contains(&format!("0x{:016x}", addr.value())), "{message}");
        assert!(message.contains("injected failure"), "{message}");
        // Tried once and retried once; the rollback's own write then needed no guard,
        // because the range was left writable.
        assert_eq!(restores.get(), 2);
        assert_eq!(page.bytes(), [1, 2, 3, 4]);
    }

    #[test]
    fn protection_restore_that_succeeds_on_retry_completes_the_write()
    {
        let page = ReadOnlyPage::new();
        let restores = Cell::new(0);
        write_with_protection_using(task(), page.address(), &[9; 4], |task, addr, len, protection| {
            restores.set(restores.get() + 1);
            if restores.get() == 1 {
                Err(failure())
            } else {
                change_protection(task, addr, len, protection)
            }
        })
        .unwrap();
        assert_eq!(restores.get(), 2);
        assert_eq!(page.bytes(), [9; 4]);
        assert_eq!(page.protection(), libc::VM_PROT_READ as u32);
    }

    #[test]
    fn write_through_a_guard_restores_the_protection()
    {
        let page = ReadOnlyPage::new();
        write_memory_all_or_nothing(task(), page.address(), &[5, 6, 7, 8]).unwrap();
        assert_eq!(page.bytes(), [5, 6, 7, 8]);
        assert_eq!(page.protection(), libc::VM_PROT_READ as u32);
    }

    #[test]
    fn successful_write_changes_memory_once()
    {
        let target = Target::new();
        let addr = target.address();
        write_memory_all_or_nothing(task(), addr, &[5, 6, 7, 8]).unwrap();
        assert_eq!(target.bytes(), [5, 6, 7, 8]);
    }

    // Issue #244: the read failures that only a map changing between the range check and
    // `mach_vm_read_overwrite` produces. No real process can be made to unmap memory in
    // that window deterministically (the target is suspended while the debugger reads, and
    // racing an unmap from another thread here would make a flaky test), so these drive
    // `read_checked_span` through its `read_chunk` seam with the kernel's answers.

    #[test]
    fn invalid_address_after_the_range_check_is_not_found_at_the_failing_chunk()
    {
        let mut dst = vec![0u8; constants::MAX_VM_READ_CHUNK + 8];
        let mut calls = Vec::new();
        let error = read_checked_span(Address::from(0x1000u64), &mut dst, |cursor, chunk| {
            calls.push((cursor, chunk.len()));
            if calls.len() == 1 {
                chunk.fill(7);
                (KERN_SUCCESS, chunk.len() as mach_vm_size_t)
            } else {
                (libc::KERN_INVALID_ADDRESS, 0)
            }
        })
        .expect_err("a chunk the kernel refused");
        let failed_at = 0x1000 + constants::MAX_VM_READ_CHUNK as u64;
        assert_eq!(calls, [(0x1000, constants::MAX_VM_READ_CHUNK), (failed_at, 8)]);
        assert!(
            matches!(&error, DebuggerError::Backend(BackendError::NotFound(message))
                if message.contains(&Address::from(failed_at).to_string())),
            "{error:?}"
        );
    }

    #[test]
    fn short_read_then_zero_bytes_is_not_found_and_does_not_loop()
    {
        let mut dst = [0u8; 16];
        let mut calls = Vec::new();
        let error = read_checked_span(Address::from(0x2000u64), &mut dst, |cursor, chunk| {
            calls.push((cursor, chunk.len()));
            match calls.len() {
                1 => {
                    chunk[..4].fill(9);
                    (KERN_SUCCESS, 4)
                }
                2 => (KERN_SUCCESS, 0),
                _ => panic!("read again after the kernel read nothing"),
            }
        })
        .expect_err("a read the kernel stopped short");
        // The short read is continued from where it stopped, then the empty one fails.
        assert_eq!(calls, [(0x2000, 16), (0x2004, 12)]);
        assert!(
            matches!(&error, DebuggerError::Backend(BackendError::NotFound(message))
                if message.contains("read 4 of 16 bytes")),
            "{error:?}"
        );
    }

    #[test]
    fn short_reads_that_complete_the_range_succeed()
    {
        let mut dst = [0u8; 8];
        let mut next = 0u8;
        let read = read_checked_span(Address::from(0x3000u64), &mut dst, |_, chunk| {
            let count = chunk.len().min(3);
            for byte in &mut chunk[..count] {
                *byte = next;
                next += 1;
            }
            (KERN_SUCCESS, count as mach_vm_size_t)
        })
        .expect("short reads that fill the buffer");
        assert_eq!(read, 8);
        assert_eq!(dst, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn read_longer_than_the_address_space_is_not_found_without_allocating()
    {
        // Without the check first, both paths try to allocate `usize::MAX` bytes, which
        // panics (capacity overflow) instead of returning an error.
        let target = Target::new();
        for error in [
            read_memory(task(), target.address(), usize::MAX).expect_err("uncached read of usize::MAX bytes"),
            MemoryCache::new()
                .read(task(), target.address(), usize::MAX)
                .expect_err("cached read of usize::MAX bytes"),
        ] {
            assert!(
                matches!(error, DebuggerError::Backend(BackendError::NotFound(_))),
                "{error:?}"
            );
        }
    }

    /// Two adjacent VM pages of this process, aligned to their combined size, whose second
    /// page is mapped but unreadable (`PROT_NONE`): one cache page of two VM pages around
    /// bytes that are readable.
    ///
    /// The second page is not unmapped: tests run in parallel, and another test's `mmap`
    /// could take the freed page before this one reads it. A mapped, unreadable page stays
    /// a hole for the read, and the cache falls back the same way whatever error the page
    /// read returned.
    struct HalfReadablePair
    {
        base: *mut libc::c_void,
        page: usize,
    }

    impl HalfReadablePair
    {
        fn new() -> Self
        {
            let page = *SYSTEM_PAGE_SIZE;
            // Map four pages so that two of them start on a two-page boundary, then keep
            // only those.
            // SAFETY: a fresh anonymous private mapping; nothing else refers to it.
            let raw = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4 * page,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANON | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            assert_ne!(raw, libc::MAP_FAILED);
            let start = raw as usize;
            let base = (start + 2 * page - 1) & !(2 * page - 1);
            // SAFETY: every range unmapped or protected here lies in the mapping above,
            // which only this value uses.
            unsafe {
                if base > start {
                    assert_eq!(libc::munmap(raw, base - start), 0);
                }
                let tail = base + 2 * page;
                if tail < start + 4 * page {
                    assert_eq!(libc::munmap(tail as *mut libc::c_void, start + 4 * page - tail), 0);
                }
                (base as *mut [u8; 4]).write_volatile([1, 2, 3, 4]);
                assert_eq!(libc::mprotect((base + page) as *mut libc::c_void, page, libc::PROT_NONE), 0);
            }
            Self {
                base: base as *mut libc::c_void,
                page,
            }
        }

        fn first(&self) -> Address
        {
            Address::from(self.base as u64)
        }

        fn second(&self) -> Address
        {
            Address::from(self.base as u64 + self.page as u64)
        }
    }

    impl Drop for HalfReadablePair
    {
        fn drop(&mut self)
        {
            // SAFETY: unmaps the two pages this value kept.
            unsafe { libc::munmap(self.base, 2 * self.page) };
        }
    }

    #[test]
    fn cache_page_larger_than_a_vm_page_reads_readable_bytes_next_to_a_hole()
    {
        let pair = HalfReadablePair::new();
        let cache = MemoryCache::with_page_size(2 * *SYSTEM_PAGE_SIZE);
        // Before the fix the whole cache page was read, which fails on the second VM page.
        let bytes = cache
            .read(task(), pair.first(), 4)
            .unwrap_or_else(|err| panic!("readable bytes next to an unreadable page: {err:?}"));
        assert_eq!(bytes, [1, 2, 3, 4]);

        // The bytes of the hole itself still fail with their own category, and so does a
        // read that runs from readable bytes into it.
        for at in [pair.second(), Address::from(pair.second().value() - 2)] {
            let error = cache.read(task(), at, 4).expect_err("read of the unreadable page");
            assert!(matches!(error, DebuggerError::PermissionDenied(_)), "at {at}: {error:?}");
        }
    }
}
