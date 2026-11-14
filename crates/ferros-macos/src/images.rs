//! Loader-reported images in the stopped target.
//!
//! `TASK_DYLD_INFO` locates the target's `dyld_all_image_infos`; remote pointers are
//! decoded as data, never dereferenced in the debugger. Before dyld's first instruction
//! its list is empty. Only at that bootstrap stop we locate the executable header in
//! the kernel's mappings, so callers can still plant breakpoints before running dyld.
//! Its path comes from the executable's vnode or, once that file is unlinked, from the
//! exec path the kernel recorded for the process; neither is inferred.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use ferros_core::backend::LoadedImage;
use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::{Address, ProcessId};
use libc::mach_port_t;
use mach2::kern_return::KERN_SUCCESS;
use mach2::task::task_info;
use mach2::task_info::{TASK_DYLD_INFO, TASK_DYLD_INFO_COUNT, task_dyld_info};
use mach2::vm::mach_vm_read_overwrite;

use crate::memory::get_memory_regions;

const MAX_IMAGES: usize = 16_384;
const MAX_PATH: usize = 4096;

fn invalid(detail: &str) -> DebuggerError
{
    DebuggerError::Os {
        code: -1,
        message: format!("dyld image list: {detail}"),
    }
}

/// The kernel validates remote ranges, including dyld paths in shared-cache submaps.
/// Require an exact read and cap allocation independently of target-controlled values.
fn read_target(task: mach_port_t, address: Address, size: usize) -> Result<Vec<u8>>
{
    if size > MAX_IMAGES * 24 || address.value().checked_add(size as u64).is_none() {
        return Err(invalid("remote read exceeds bounds"));
    }
    let mut bytes = vec![0; size];
    let mut actual = 0;
    // SAFETY: bytes is writable for size bytes; address is an integer in the target
    // task. Mach copies into our allocation and checks the target mappings itself.
    let result =
        unsafe { mach_vm_read_overwrite(task, address.value(), size as u64, bytes.as_mut_ptr() as u64, &mut actual) };
    if result != KERN_SUCCESS || actual != size as u64 {
        return Err(DebuggerError::Os {
            code: i64::from(result),
            message: format!("reading dyld data at {:#x}: requested {size}, read {actual}", address.value()),
        });
    }
    Ok(bytes)
}

fn word(bytes: &[u8], offset: usize) -> u64
{
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("validated remote record"))
}

fn array_header(bytes: &[u8]) -> Result<(usize, u64)>
{
    if bytes.len() != 16 {
        return Err(invalid("truncated header"));
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().expect("header size")) as usize;
    if count > MAX_IMAGES {
        return Err(invalid("image count exceeds limit"));
    }
    Ok((count, word(bytes, 8)))
}

fn remote_path(task: mach_port_t, mut address: u64) -> Result<PathBuf>
{
    if address == 0 {
        return Err(invalid("null image path"));
    }
    let mut path = Vec::new();
    while path.len() < MAX_PATH {
        // Do not cross a page boundary just to read a short string at a page's end.
        let size = (4096 - address as usize % 4096).min(256).min(MAX_PATH - path.len());
        let bytes = read_target(task, Address::new(address), size)
            .map_err(|error| invalid(&format!("path at {address:#x} size {size}: {error}")))?;
        if let Some(end) = bytes.iter().position(|&byte| byte == 0) {
            path.extend_from_slice(&bytes[..end]);
            return Ok(PathBuf::from(OsString::from_vec(path)));
        }
        path.extend_from_slice(&bytes);
        address = address
            .checked_add(size as u64)
            .ok_or_else(|| invalid("path address overflow"))?;
    }
    Err(invalid("unterminated image path"))
}

/// The path of the executable's vnode. It is unavailable once that file is unlinked.
fn vnode_path(pid: ProcessId) -> Option<PathBuf>
{
    libproc::libproc::proc_pid::pidpath(pid.0 as i32).ok().map(PathBuf::from)
}

/// The exec path the kernel stored with the process's arguments (`KERN_PROCARGS2`), as
/// dyld receives it. Only an absolute path is returned: a relative one would need a
/// working directory that may have changed since exec.
fn exec_path(pid: ProcessId) -> Option<PathBuf>
{
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid.0 as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: mib holds three valid ints; a null buffer asks the kernel only for the size.
    let result = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) };
    if result != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: buffer is writable for size bytes; the kernel writes at most size bytes and
    // stores the number written in size.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    parse_exec_path(buffer.get(..size)?)
}

/// `KERN_PROCARGS2` is `argc` (an `int`) followed by the NUL-terminated exec path.
fn parse_exec_path(procargs: &[u8]) -> Option<PathBuf>
{
    let rest = procargs.get(size_of::<libc::c_int>()..)?;
    let path = &rest[..rest.iter().position(|&byte| byte == 0)?];
    (path.first() == Some(&b'/')).then(|| PathBuf::from(OsString::from_vec(path.to_vec())))
}

/// Whether the 64-bit Mach-O header at `address` is the main executable's (`MH_EXECUTE`).
fn is_executable_header(task: mach_port_t, address: Address) -> bool
{
    read_target(task, address, 16)
        .is_ok_and(|bytes| bytes[..4] == 0xfeed_facf_u32.to_le_bytes() && bytes[12..16] == 2_u32.to_le_bytes())
}

/// Compatibility for `POSIX_SPAWN_START_SUSPENDED`: dyld has not populated its array.
/// No inferred slide is used: the actual mapped Mach-O header is returned.
fn bootstrap_executable(task: mach_port_t, pid: ProcessId) -> Result<Vec<LoadedImage>>
{
    let path = vnode_path(pid)
        .or_else(|| exec_path(pid))
        .ok_or_else(|| invalid("cannot locate main executable path"))?;
    get_memory_regions(task)?
        .iter()
        .filter(|region| region.is_readable())
        .find(|region| is_executable_header(task, region.start))
        .map(|region| {
            vec![LoadedImage {
                path,
                load_address: region.start,
            }]
        })
        .ok_or_else(|| invalid("loader is uninitialized and the executable header is unavailable"))
}

pub(crate) fn loaded_images(task: mach_port_t, pid: ProcessId) -> Result<Vec<LoadedImage>>
{
    let mut info = task_dyld_info::default();
    let mut count = TASK_DYLD_INFO_COUNT;
    // SAFETY: info is initialized and writable for TASK_DYLD_INFO_COUNT natural_t
    // words. The kernel fills it; it contains integer target addresses, not local pointers.
    let result = unsafe { task_info(task, TASK_DYLD_INFO, (&raw mut info).cast(), &mut count) };
    if result != KERN_SUCCESS {
        return Err(DebuggerError::Os {
            code: i64::from(result),
            message: "task_info(TASK_DYLD_INFO) failed".into(),
        });
    }
    if info.all_image_info_addr == 0 {
        return bootstrap_executable(task, pid);
    }
    if info.all_image_info_format != 1 || info.all_image_info_size < 16 {
        return Err(invalid("unsupported image-info format"));
    }
    let address = Address::new(info.all_image_info_addr);
    let before = read_target(task, address, 16)?;
    let (length, array) = array_header(&before)?;
    if length == 0 {
        return bootstrap_executable(task, pid);
    }
    if array == 0 {
        return Err(invalid("loader is modifying the image list; resume to a stable stop"));
    }
    let size = length.checked_mul(24).ok_or_else(|| invalid("array size overflow"))?;
    array
        .checked_add(size as u64)
        .ok_or_else(|| invalid("array address overflow"))?;
    let records = read_target(task, Address::new(array), size)
        .map_err(|error| invalid(&format!("array at {array:#x} size {size}: {error}")))?;
    if records.len() != size {
        return Err(invalid("truncated image array"));
    }
    let mut images = Vec::with_capacity(length);
    for record in records.as_chunks::<24>().0 {
        let load_address = word(record, 0);
        if load_address == 0 {
            return Err(invalid("null image load address"));
        }
        images.push(LoadedImage {
            path: remote_path(task, word(record, 8))?,
            load_address: Address::new(load_address),
        });
    }
    if read_target(task, address, 16)? != before {
        return Err(invalid("image list changed while reading"));
    }
    // Preserve the existing main-first contract for callers; dyld's order is not an API.
    // The main image is found by its header, so this holds after its file is unlinked.
    if let Some(index) = images.iter().position(|image| is_executable_header(task, image.load_address)) {
        images.swap(0, index);
    }
    Ok(images)
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn rejects_truncated_and_unbounded_image_lists()
    {
        assert!(array_header(&[0; 15]).is_err());
        let mut header = [0; 16];
        header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(array_header(&header).is_err());
        header[4..8].copy_from_slice(&2_u32.to_le_bytes());
        header[8..].copy_from_slice(&0x1234_u64.to_le_bytes());
        assert_eq!(array_header(&header).expect("header"), (2, 0x1234));
    }

    #[test]
    fn exec_path_is_the_absolute_path_after_argc()
    {
        let procargs = |path: &[u8]| [&2_i32.to_ne_bytes()[..], path, b"\0\0\0arg\0"].concat();
        assert_eq!(parse_exec_path(&procargs(b"/bin/ls")), Some(PathBuf::from("/bin/ls")));
        assert_eq!(parse_exec_path(&procargs(b"ls")), None);
        assert_eq!(parse_exec_path(&procargs(b"")), None);
        assert_eq!(parse_exec_path(b"/no"), None);
        assert_eq!(parse_exec_path(&[0, 0, 0, 0, b'/', b'x']), None);
    }
}
