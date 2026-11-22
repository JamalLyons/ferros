//! Loader-reported images in the stopped target.
//!
//! `TASK_DYLD_INFO` locates the target's `dyld_all_image_infos`; remote pointers are
//! decoded as data, never dereferenced in the debugger. Before dyld's first instruction
//! its list is empty. Only at that bootstrap stop we locate the executable header in
//! the kernel's mappings, so callers can still plant breakpoints before running dyld.
//! Its path comes from the executable's vnode or, once that file is unlinked, from the
//! exec path the kernel recorded for the process; neither is inferred.
//!
//! dyld leaves itself out of the array it publishes. It is listed after the array's
//! images from `dyldImageLoadAddress` in the same structure (and `dyldPath`, when the
//! structure is new enough to have it), once its header there reads as `MH_DYLINKER`.
//! At the bootstrap stop, before dyld has set those fields, it is the `MH_DYLINKER`
//! header the kernel mapped; dyld may later move itself (into the shared cache), and the
//! list then follows the address dyld records.
//!
//! Each image's Mach-O UUID comes from `uuidArray` in that same structure (version 8 and
//! later): one `dyld_uuid_info` per image that is not in the dyld shared cache, matched
//! by load address. A missing, oversized or unreadable array leaves every UUID unset and
//! still returns the list. Images the array does not cover stay `None`: shared-cache
//! libraries, dyld when it lives in the cache, and the executable at the bootstrap stop
//! before dyld publishes the array. Their `LC_UUID` is in the mapped header, but reading
//! it for every shared-cache image is a remote read each, so it is not done.

use std::collections::HashMap;
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

/// `MH_EXECUTE`, the main executable's Mach-O file type.
const MH_EXECUTE: u32 = 2;

/// `MH_DYLINKER`, dyld's Mach-O file type.
const MH_DYLINKER: u32 = 7;

/// Whether a 64-bit Mach-O header of `filetype` is mapped at `address`.
fn is_header_of_type(task: mach_port_t, address: Address, filetype: u32) -> bool
{
    read_target(task, address, 16)
        .is_ok_and(|bytes| bytes[..4] == 0xfeed_facf_u32.to_le_bytes() && bytes[12..16] == filetype.to_le_bytes())
}

/// Whether the 64-bit Mach-O header at `address` is the main executable's (`MH_EXECUTE`).
fn is_executable_header(task: mach_port_t, address: Address) -> bool
{
    is_header_of_type(task, address, MH_EXECUTE)
}

/// Offset of `dyldImageLoadAddress` in the 64-bit `dyld_all_image_infos` (version 2+).
const DYLD_LOAD_ADDRESS_OFFSET: usize = 32;

/// Offset of `dyldPath` in the 64-bit `dyld_all_image_infos` (version 15+).
const DYLD_PATH_OFFSET: usize = 192;

/// Offset of `uuidArrayCount` in the 64-bit `dyld_all_image_infos` (version 8+).
const UUID_ARRAY_COUNT_OFFSET: usize = 88;

/// Offset of `uuidArray` in the 64-bit `dyld_all_image_infos` (version 8+).
const UUID_ARRAY_OFFSET: usize = 96;

/// `struct dyld_uuid_info`: an 8-byte load address, then `uuid_t` (16 bytes).
const UUID_INFO_SIZE: usize = 24;

/// First `dyld_all_image_infos` version that records `uuidArray`.
const UUID_ARRAY_VERSION: u32 = 8;

/// Bytes of `dyld_all_image_infos` read: everything up to and including `dyldPath`.
const ALL_IMAGE_INFOS_PREFIX: usize = DYLD_PATH_OFFSET + 8;

/// Where dyld lives when the structure does not record its path.
const DEFAULT_DYLD_PATH: &str = "/usr/lib/dyld";

/// dyld's own entry in the prefix `bytes` of `dyld_all_image_infos`: its load address
/// and the remote address of its path (`0` when the structure has none). `None` when the
/// structure is too old or too short to record dyld, or dyld has not set its address.
fn dyld_fields(bytes: &[u8]) -> Option<(u64, u64)>
{
    let version = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    if version < 2 || bytes.len() < DYLD_LOAD_ADDRESS_OFFSET + 8 {
        return None;
    }
    let load_address = word(bytes, DYLD_LOAD_ADDRESS_OFFSET);
    if load_address == 0 {
        return None;
    }
    let path = if version >= 15 && bytes.len() >= DYLD_PATH_OFFSET + 8 {
        word(bytes, DYLD_PATH_OFFSET)
    } else {
        0
    };
    Some((load_address, path))
}

/// `uuidArrayCount` and `uuidArray` in a prefix of the 64-bit `dyld_all_image_infos`.
///
/// `None` when `bytes` is older than version 8, shorter than the pointer, or the count is
/// zero, the pointer is null, or the count is above [`MAX_IMAGES`]. A bad count leaves
/// UUIDs unset rather than failing the image list. The count is small enough that that
/// many [`UUID_INFO_SIZE`]-byte records fit at the returned address.
fn uuid_array_fields(bytes: &[u8]) -> Option<(usize, u64)>
{
    let version = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    if version < UUID_ARRAY_VERSION || bytes.len() < UUID_ARRAY_OFFSET + 8 {
        return None;
    }
    let count = usize::try_from(word(bytes, UUID_ARRAY_COUNT_OFFSET)).ok()?;
    let array = word(bytes, UUID_ARRAY_OFFSET);
    if count == 0 || array == 0 || count > MAX_IMAGES {
        return None;
    }
    let size = count.checked_mul(UUID_INFO_SIZE)?;
    array.checked_add(u64::try_from(size).ok()?)?;
    Some((count, array))
}

/// `dyld_uuid_info` records. A trailing partial record is ignored. A null load address
/// is not an image and is skipped.
fn decode_uuid_infos(records: &[u8]) -> Vec<(u64, [u8; 16])>
{
    let mut found = Vec::new();
    for record in records.as_chunks::<UUID_INFO_SIZE>().0 {
        let load_address = word(record, 0);
        if load_address == 0 {
            continue;
        }
        let uuid = record[8..24].try_into().expect("dyld_uuid_info uuid is 16 bytes");
        found.push((load_address, uuid));
    }
    found
}

/// UUIDs from `uuidArray`, keyed by load address. A failed or inconsistent read yields an
/// empty map: the image list is still returned, with UUIDs unset.
fn image_uuids(task: mach_port_t, infos: &[u8]) -> HashMap<u64, [u8; 16]>
{
    let Some((count, array)) = uuid_array_fields(infos) else {
        return HashMap::new();
    };
    let Some(size) = count.checked_mul(UUID_INFO_SIZE) else {
        return HashMap::new();
    };
    let records = match read_target(task, Address::new(array), size) {
        Ok(records) if records.len() == size => records,
        Ok(_) => {
            tracing::debug!("dyld uuid array at {array:#x} was truncated; leaving UUIDs unset");
            return HashMap::new();
        }
        Err(error) => {
            tracing::debug!("dyld uuid array at {array:#x}: {error}; leaving UUIDs unset");
            return HashMap::new();
        }
    };
    decode_uuid_infos(&records).into_iter().collect()
}

fn with_uuid(path: PathBuf, load_address: Address, uuids: &HashMap<u64, [u8; 16]>) -> LoadedImage
{
    LoadedImage {
        path,
        load_address,
        uuid: uuids.get(&load_address.value()).copied(),
    }
}

/// dyld as a loaded image, from the prefix `bytes` of `dyld_all_image_infos`, unless
/// `images` already holds its load address. Only a mapped `MH_DYLINKER` header at the
/// recorded address is listed; anything else leaves dyld out rather than failing the list.
fn dyld_image(task: mach_port_t, bytes: &[u8], images: &[LoadedImage], uuids: &HashMap<u64, [u8; 16]>)
-> Option<LoadedImage>
{
    let (load_address, path_address) = dyld_fields(bytes)?;
    let load_address = Address::new(load_address);
    if images.iter().any(|image| image.load_address == load_address) {
        return None;
    }
    if !is_header_of_type(task, load_address, MH_DYLINKER) {
        tracing::trace!("dyld's recorded load address {load_address} holds no MH_DYLINKER header");
        return None;
    }
    let path = if path_address == 0 {
        PathBuf::from(DEFAULT_DYLD_PATH)
    } else {
        remote_path(task, path_address).unwrap_or_else(|error| {
            tracing::debug!("dyld's path at {path_address:#x}: {error}; using {DEFAULT_DYLD_PATH}");
            PathBuf::from(DEFAULT_DYLD_PATH)
        })
    };
    Some(with_uuid(path, load_address, uuids))
}

/// Compatibility for `POSIX_SPAWN_START_SUSPENDED`: dyld has not populated its array.
/// No inferred slide is used: the actual mapped Mach-O headers are returned.
///
/// The main executable is found by its `MH_EXECUTE` header at a region's start. dyld is
/// listed after it from `infos` (the prefix of `dyld_all_image_infos`, if there is one)
/// when that records a mapped dyld header; before dyld relocates itself those fields are
/// not yet usable, so otherwise dyld is the `MH_DYLINKER` header the kernel mapped at a
/// region's start, under [`DEFAULT_DYLD_PATH`].
fn bootstrap_images(task: mach_port_t, pid: ProcessId, infos: Option<&[u8]>) -> Result<Vec<LoadedImage>>
{
    let path = vnode_path(pid)
        .or_else(|| exec_path(pid))
        .ok_or_else(|| invalid("cannot locate main executable path"))?;
    let uuids = infos.map(|infos| image_uuids(task, infos)).unwrap_or_default();
    let regions = get_memory_regions(task)?;
    let mut readable = regions.iter().filter(|region| region.is_readable());
    let executable = readable
        .clone()
        .find(|region| is_executable_header(task, region.start))
        .ok_or_else(|| invalid("loader is uninitialized and the executable header is unavailable"))?;
    let mut images = vec![with_uuid(path, executable.start, &uuids)];
    let dyld = infos.and_then(|infos| dyld_image(task, infos, &images, &uuids)).or_else(|| {
        readable
            .find(|region| is_header_of_type(task, region.start, MH_DYLINKER))
            .map(|region| with_uuid(PathBuf::from(DEFAULT_DYLD_PATH), region.start, &uuids))
    });
    images.extend(dyld);
    Ok(images)
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
        return bootstrap_images(task, pid, None);
    }
    if info.all_image_info_format != 1 || info.all_image_info_size < 16 {
        return Err(invalid("unsupported image-info format"));
    }
    let address = Address::new(info.all_image_info_addr);
    let before = read_target(task, address, 16)?;
    let (length, array) = array_header(&before)?;
    // The fields past the array header, up to dyld's path, as far as the target's
    // structure reaches. A failed read only leaves dyld unlisted.
    let prefix =
        usize::try_from(info.all_image_info_size).map_or(ALL_IMAGE_INFOS_PREFIX, |size| size.min(ALL_IMAGE_INFOS_PREFIX));
    let infos = read_target(task, address, prefix).unwrap_or_else(|error| {
        tracing::debug!("dyld's own image fields: {error}");
        before.clone()
    });
    if length == 0 {
        return bootstrap_images(task, pid, Some(&infos));
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
            uuid: None,
        });
    }
    // Read before the stability check below, so a list that changed under it is refused
    // too. A failed UUID read must not fail the list. Shared-cache images are absent here.
    let uuids = image_uuids(task, &infos);
    if read_target(task, address, 16)? != before {
        return Err(invalid("image list changed while reading"));
    }
    for image in &mut images {
        image.uuid = uuids.get(&image.load_address.value()).copied();
    }
    // Preserve the existing main-first contract for callers; dyld's order is not an API.
    // The main image is found by its header, so this holds after its file is unlinked.
    if let Some(index) = images.iter().position(|image| is_executable_header(task, image.load_address)) {
        images.swap(0, index);
    }
    images.extend(dyld_image(task, &infos, &images, &uuids));
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

    /// A `dyld_all_image_infos` prefix of `len` bytes at `version`, recording dyld at
    /// `load` with its path at `path`.
    fn infos(version: u32, len: usize, load: u64, path: u64) -> Vec<u8>
    {
        let mut bytes = vec![0; ALL_IMAGE_INFOS_PREFIX];
        bytes[..4].copy_from_slice(&version.to_le_bytes());
        bytes[DYLD_LOAD_ADDRESS_OFFSET..DYLD_LOAD_ADDRESS_OFFSET + 8].copy_from_slice(&load.to_le_bytes());
        bytes[DYLD_PATH_OFFSET..DYLD_PATH_OFFSET + 8].copy_from_slice(&path.to_le_bytes());
        bytes.truncate(len);
        bytes
    }

    #[test]
    fn dyld_fields_follow_the_structure_version_and_length()
    {
        let full = ALL_IMAGE_INFOS_PREFIX;
        assert_eq!(dyld_fields(&infos(17, full, 0x1000, 0x2000)), Some((0x1000, 0x2000)));
        assert_eq!(dyld_fields(&infos(15, full, 0x1000, 0x2000)), Some((0x1000, 0x2000)));
        // Before version 15 there is no `dyldPath`, whatever the bytes there hold.
        assert_eq!(dyld_fields(&infos(14, full, 0x1000, 0x2000)), Some((0x1000, 0)));
        // A structure that ends before `dyldPath` still records dyld's address.
        assert_eq!(dyld_fields(&infos(17, full - 1, 0x1000, 0x2000)), Some((0x1000, 0)));
        assert_eq!(dyld_fields(&infos(2, 40, 0x1000, 0x2000)), Some((0x1000, 0)));
        // Version 1, a structure too short for the address, or an unset address: no dyld.
        assert_eq!(dyld_fields(&infos(1, full, 0x1000, 0x2000)), None);
        assert_eq!(dyld_fields(&infos(17, 39, 0x1000, 0x2000)), None);
        assert_eq!(dyld_fields(&infos(17, 16, 0x1000, 0x2000)), None);
        assert_eq!(dyld_fields(&infos(17, full, 0, 0x2000)), None);
        assert_eq!(dyld_fields(&[]), None);
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

    /// Write `count` and `array` into a version-`version` prefix of `len` bytes.
    fn uuid_infos(version: u32, len: usize, count: u64, array: u64) -> Vec<u8>
    {
        let mut bytes = infos(version, ALL_IMAGE_INFOS_PREFIX, 0, 0);
        bytes[UUID_ARRAY_COUNT_OFFSET..UUID_ARRAY_COUNT_OFFSET + 8].copy_from_slice(&count.to_le_bytes());
        bytes[UUID_ARRAY_OFFSET..UUID_ARRAY_OFFSET + 8].copy_from_slice(&array.to_le_bytes());
        bytes.truncate(len);
        bytes
    }

    #[test]
    fn uuid_array_fields_follow_version_length_and_the_count_cap()
    {
        let full = ALL_IMAGE_INFOS_PREFIX;
        assert_eq!(uuid_array_fields(&uuid_infos(8, full, 2, 0x4000)), Some((2, 0x4000)));
        assert_eq!(uuid_array_fields(&uuid_infos(17, full, 1, 0x4000)), Some((1, 0x4000)));
        // Version 7 has no `uuidArray`, whatever those bytes hold.
        assert_eq!(uuid_array_fields(&uuid_infos(7, full, 2, 0x4000)), None);
        // The pointer does not fit in a shorter prefix.
        assert_eq!(uuid_array_fields(&uuid_infos(8, UUID_ARRAY_OFFSET + 7, 2, 0x4000)), None);
        assert_eq!(uuid_array_fields(&uuid_infos(8, full, 0, 0x4000)), None);
        assert_eq!(uuid_array_fields(&uuid_infos(8, full, 2, 0)), None);
        assert_eq!(uuid_array_fields(&uuid_infos(8, full, (MAX_IMAGES as u64) + 1, 0x4000)), None);
        assert_eq!(uuid_array_fields(&[]), None);
    }

    #[test]
    fn decode_uuid_infos_reads_load_address_and_uuid()
    {
        let mut records = Vec::new();
        records.extend_from_slice(&0x1000_u64.to_le_bytes());
        records.extend_from_slice(&[0xAB; 16]);
        // A null load address is not an image.
        records.extend_from_slice(&0_u64.to_le_bytes());
        records.extend_from_slice(&[0xCD; 16]);
        records.extend_from_slice(&0x2000_u64.to_le_bytes());
        records.extend_from_slice(&[0x11; 16]);
        // A trailing partial record is ignored.
        records.extend_from_slice(&[0xFF; 8]);
        assert_eq!(decode_uuid_infos(&records), vec![(0x1000, [0xAB; 16]), (0x2000, [0x11; 16])]);
        assert!(decode_uuid_infos(&[]).is_empty());
    }
}
