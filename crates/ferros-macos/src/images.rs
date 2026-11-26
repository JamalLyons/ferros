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
//! In the #270 live probe on macOS 27.2.1 (26B131, arm64), the `hello` fixture
//! loaded a library via relative `DYLD_INSERT_LIBRARIES` entries in two orders
//! and first loaded it via `@rpath` in a third launch. In each launch, `dlopen`
//! also tried absolute, relative, symlink and hard-link paths. `Session::images()`
//! at the fixture's `hello_print` marker returned 86 rows at 86 distinct addresses.
//! All spellings returned the same base from
//! `dladdr` and one row with the resolved path. A separate library with the same
//! install name and a different UUID had its own row and address in every launch.
//!
//! Apple dyld 1378 resolves aliases to existing loaders by path, file identity or
//! shared-cache index. It skips duplicate startup insertions and publishes one path
//! and header address per newly reported loader. Ferros reads those paths directly.
//! Disk images reserve their own virtual address ranges, and shared-cache images
//! use their mapped header addresses.
//! See [loader lookup], [VM reservation], [startup insertion], [load notification],
//! and [list publication].
//!
//! [loader lookup]: https://github.com/apple-oss-distributions/dyld/blob/dyld-1378/dyld/Loader.cpp#L763-L1040
//! [VM reservation]: https://github.com/apple-oss-distributions/dyld/blob/dyld-1378/dyld/Loader.cpp#L1663-L1670
//! [startup insertion]: https://github.com/apple-oss-distributions/dyld/blob/dyld-1378/dyld/DyldRuntimeState.cpp#L646-L664
//! [load notification]: https://github.com/apple-oss-distributions/dyld/blob/dyld-1378/dyld/DyldRuntimeState.cpp#L1716-L1742
//! [list publication]: https://github.com/apple-oss-distributions/dyld/blob/dyld-1378/dyld/ExternallyViewableState.cpp#L456-L500
//!
//! Each image's Mach-O UUID comes from `uuidArray` in that same structure (version 8 and
//! later), matched by load address. For images absent from the array, including shared-cache
//! libraries, dyld and the executable at the bootstrap stop, read `LC_UUID` from the mapped
//! load commands. Both reads are bounded, and a failed UUID lookup still returns the image (#170).
//!
//! A mapped header does not change while that image stays mapped, so a successful read is
//! cached by load address (#182). The entry is used only when the loader still reports the
//! same path there. Every entry is dropped when `infoArrayChangeTimestamp` changes, or, when
//! that field is absent, when the path and address list changes, so a different image at the
//! same address is read again. A failed read is not cached.
//!
//! #170 added those two remote reads for every image the UUID array omits, on every
//! `images()` call (`Session::load_images` does that for each backtrace, address mapping,
//! and step-out). The integration test `measure_mapped_uuid_and_backtrace_cost` times
//! `images()` and a deep backtrace with the reads skipped (before #170), repeated (after
//! #170), and cached. On the macOS runner it writes the timings to
//! `target/ferros-uuid-measure.txt`.
//!
//! One run of that test on the GitHub `integration-macos` runner (macOS 15, arm64), a
//! stopped `recursion` process with 45 images and a 77-frame trace, medians of five
//! `images()` calls and three traces: 72 µs and 0 header reads before #170, 246 µs and
//! 88 header reads after, 103 µs and 0 header reads once the cache was warm. The trace
//! was 1590 µs, 1747 µs, and 1590 µs for those three. A later run prints its own line.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use ferros_core::backend::{ImageRole, LoadedImage};
use ferros_core::error::{DebuggerError, Result};
use ferros_core::types::{Address, BuildId, BuildIdentity, ProcessId};
use libc::mach_port_t;
use mach2::kern_return::KERN_SUCCESS;
use mach2::task::task_info;
use mach2::task_info::{TASK_DYLD_INFO, TASK_DYLD_INFO_COUNT, task_dyld_info};
use mach2::vm::mach_vm_read_overwrite;
use object::Endianness;
use object::macho::MachHeader64;
use object::read::macho::MachHeader;

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

/// Offset of `infoArrayChangeTimestamp` in the 64-bit `dyld_all_image_infos` (version 15+).
///
/// The `uint64_t` immediately before [`DYLD_PATH_OFFSET`]. Dyld updates it when the image
/// list changes, including when a different image is mapped at an address already in use.
const INFO_ARRAY_CHANGE_TIMESTAMP_OFFSET: usize = 184;

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
/// zero, the pointer is null, or the count is above [`MAX_IMAGES`]. The returned address
/// range fits that many [`UUID_INFO_SIZE`]-byte records without overflowing.
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

/// `infoArrayChangeTimestamp` when `infos` is long enough to be a version-15 structure.
///
/// `None` for an older or shorter prefix: those builds have no list generation, and the
/// cache then compares paths and addresses instead.
fn image_list_timestamp(infos: &[u8]) -> Option<u64>
{
    let version = u32::from_le_bytes(infos.get(..4)?.try_into().ok()?);
    if version < 15 || infos.len() < DYLD_PATH_OFFSET {
        return None;
    }
    Some(word(infos, INFO_ARRAY_CHANGE_TIMESTAMP_OFFSET))
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
/// empty map, so UUID lookup can fall back to mapped headers.
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
            tracing::debug!("dyld uuid array at {array:#x} was truncated");
            return HashMap::new();
        }
        Err(error) => {
            tracing::debug!("dyld uuid array at {array:#x}: {error}");
            return HashMap::new();
        }
    };
    decode_uuid_infos(&records).into_iter().collect()
}

/// How `images()` obtains a UUID the dyld array does not already have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UuidLookup
{
    /// Read the mapped header and remember a successful parse (#182).
    Cache,
    /// Read the mapped header on every call and ignore the cache. This is the #170 cost.
    Always,
    /// Do not read mapped headers. This is the cost from before #170.
    Never,
}

/// Outcome of one mapped `LC_UUID` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MappedUuidRead
{
    /// The header parsed. `None` means it has no `LC_UUID`.
    Parsed(Option<[u8; 16]>),
    /// The remote read or the parse failed. Not cached, and not fatal to the image list.
    Failed,
}

impl MappedUuidRead
{
    fn parsed(self) -> Option<[u8; 16]>
    {
        match self {
            Self::Parsed(uuid) => uuid,
            Self::Failed => None,
        }
    }
}

/// One cached mapped UUID. The path is part of the key: a different image at the same
/// address must not reuse it.
struct CachedUuid
{
    path: PathBuf,
    uuid: Option<[u8; 16]>,
}

/// Mapped `LC_UUID` values for the current task, keyed by load address.
#[derive(Default)]
pub(crate) struct MappedUuidCache
{
    entries: HashMap<u64, CachedUuid>,
    /// Last `infoArrayChangeTimestamp` applied. `None` until a timestamped list is seen
    /// and after [`clear`](Self::clear).
    timestamp: Option<u64>,
    /// Last path and address list, compared when dyld records no timestamp.
    paths: Vec<(u64, PathBuf)>,
    /// [`invalidate_mapped_uuid_cache`] generation last applied to `entries`.
    test_epoch: u64,
}

impl MappedUuidCache
{
    /// Drop every cached UUID. Used when this backend takes a new task or loses the one
    /// it had, so a reused address cannot report the previous process's image.
    pub(crate) fn clear(&mut self)
    {
        self.entries.clear();
        self.timestamp = None;
        self.paths.clear();
    }

    /// Remember `images` as the current list. A new timestamp, or a different path and
    /// address list when there is no timestamp, drops every entry first.
    fn begin(&mut self, timestamp: Option<u64>, images: &[LoadedImage])
    {
        let changed = match timestamp {
            Some(timestamp) => self.timestamp != Some(timestamp),
            None => self.paths_differ(images),
        };
        if changed {
            self.entries.clear();
        }
        self.timestamp = timestamp;
        if timestamp.is_none() {
            self.paths = images
                .iter()
                .map(|image| (image.load_address.value(), image.path.clone()))
                .collect();
        }
    }

    fn paths_differ(&self, images: &[LoadedImage]) -> bool
    {
        self.paths.len() != images.len()
            || self
                .paths
                .iter()
                .zip(images)
                .any(|((address, path), image)| *address != image.load_address.value() || path != &image.path)
    }

    /// A cached UUID for `path` at `address`, including a confirmed absence. `None` when
    /// this address is unknown or holds a different path.
    fn get(&self, address: u64, path: &Path) -> Option<Option<[u8; 16]>>
    {
        let cached = self.entries.get(&address)?;
        (cached.path == path).then_some(cached.uuid)
    }

    fn insert(&mut self, address: u64, path: PathBuf, uuid: Option<[u8; 16]>)
    {
        self.entries.insert(address, CachedUuid { path, uuid });
    }

    fn retain(&mut self, addresses: &[u64])
    {
        self.entries.retain(|address, _| addresses.contains(address));
    }
}

/// Read only the mapped header and load commands, with the read cap in `read_target`.
///
/// Each remote read counts toward [`mapped_uuid_header_reads`]. A parsed header is
/// [`MappedUuidRead::Parsed`] even when it has no `LC_UUID`; a failed read or parse is
/// [`MappedUuidRead::Failed`] and must not be cached.
fn mapped_uuid(task: mach_port_t, address: Address) -> MappedUuidRead
{
    note_mapped_uuid_header_read();
    let header = match read_target(task, address, crate::macho::MACH_HEADER_SIZE) {
        Ok(header) => header,
        Err(_) => return MappedUuidRead::Failed,
    };
    let size = match crate::macho::header_size(&header) {
        Ok(size) => size,
        Err(_) => return MappedUuidRead::Failed,
    };
    note_mapped_uuid_header_read();
    let bytes = match read_target(task, address, size) {
        Ok(bytes) => bytes,
        Err(_) => return MappedUuidRead::Failed,
    };
    let Ok(mach) = MachHeader64::<Endianness>::parse(&*bytes, 0) else {
        return MappedUuidRead::Failed;
    };
    let Ok(endian) = mach.endian() else {
        return MappedUuidRead::Failed;
    };
    match mach.uuid(endian, &*bytes, 0) {
        Ok(uuid) => MappedUuidRead::Parsed(uuid),
        Err(_) => MappedUuidRead::Failed,
    }
}

/// UUID for one image. The dyld array wins. Otherwise the cache, then one mapped read.
fn resolve_mapped_uuid(
    address: u64,
    path: &Path,
    dyld_uuid: Option<[u8; 16]>,
    cache: &mut MappedUuidCache,
    read_mapped: &mut impl FnMut(u64) -> MappedUuidRead,
) -> Option<[u8; 16]>
{
    if let Some(uuid) = dyld_uuid {
        return Some(uuid);
    }
    if let Some(uuid) = cache.get(address, path) {
        return uuid;
    }
    match read_mapped(address) {
        MappedUuidRead::Parsed(uuid) => {
            cache.insert(address, path.to_path_buf(), uuid);
            uuid
        }
        MappedUuidRead::Failed => None,
    }
}

/// The build identity reported for a Mach-O UUID lookup. A missing UUID is always
/// [`BuildIdentity::Unknown`], never `Absent`: dyld's array may omit the image, a header
/// read may fail at one stop and succeed at the next (#170), and the session must keep
/// the image's mapping and symbols across such a stop (#169, #175).
fn build_identity(uuid: Option<[u8; 16]>) -> BuildIdentity
{
    uuid.map_or(BuildIdentity::Unknown, |uuid| BuildIdentity::Known(BuildId::from(uuid)))
}

/// Fill `images` from the cache, after dropping entries the new list invalidates.
fn assign_cached_uuids(
    images: &mut [LoadedImage],
    dyld_uuids: &HashMap<u64, [u8; 16]>,
    cache: &mut MappedUuidCache,
    timestamp: Option<u64>,
    mut read_mapped: impl FnMut(u64) -> MappedUuidRead,
)
{
    cache.begin(timestamp, images);
    for image in &mut *images {
        let address = image.load_address.value();
        image.build_id = build_identity(resolve_mapped_uuid(
            address,
            &image.path,
            dyld_uuids.get(&address).copied(),
            cache,
            &mut read_mapped,
        ));
    }
    let addresses: Vec<u64> = images.iter().map(|image| image.load_address.value()).collect();
    cache.retain(&addresses);
}

fn fill_uuids(
    task: mach_port_t,
    images: &mut [LoadedImage],
    dyld_uuids: &HashMap<u64, [u8; 16]>,
    cache: &mut MappedUuidCache,
    lookup: UuidLookup,
    timestamp: Option<u64>,
)
{
    match lookup {
        UuidLookup::Never => {
            for image in &mut *images {
                image.build_id = build_identity(dyld_uuids.get(&image.load_address.value()).copied());
            }
        }
        UuidLookup::Always => {
            for image in &mut *images {
                let address = image.load_address;
                image.build_id = build_identity(
                    dyld_uuids
                        .get(&address.value())
                        .copied()
                        .or_else(|| mapped_uuid(task, address).parsed()),
                );
            }
        }
        UuidLookup::Cache => assign_cached_uuids(images, dyld_uuids, cache, timestamp, |address| {
            mapped_uuid(task, Address::new(address))
        }),
    }
}

/// dyld as a loaded image, from the prefix `bytes` of `dyld_all_image_infos`, unless
/// `images` already holds its load address. Only a mapped `MH_DYLINKER` header at the
/// recorded address is listed; anything else leaves dyld out rather than failing the list.
/// The UUID is filled later with the rest of the list.
fn dyld_image(task: mach_port_t, bytes: &[u8], images: &[LoadedImage]) -> Option<LoadedImage>
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
    Some(LoadedImage {
        path,
        load_address,
        build_id: BuildIdentity::Unknown,
        role: ImageRole::Library,
    })
}

/// Compatibility for `POSIX_SPAWN_START_SUSPENDED`: dyld has not populated its array.
/// No inferred slide is used: the actual mapped Mach-O headers are returned.
///
/// The main executable is found by its `MH_EXECUTE` header at a region's start. dyld is
/// listed after it from `infos` (the prefix of `dyld_all_image_infos`, if there is one)
/// when that records a mapped dyld header; before dyld relocates itself those fields are
/// not yet usable, so otherwise dyld is the `MH_DYLINKER` header the kernel mapped at a
/// region's start, under [`DEFAULT_DYLD_PATH`].
fn bootstrap_images(
    task: mach_port_t,
    pid: ProcessId,
    infos: Option<&[u8]>,
    cache: &mut MappedUuidCache,
    lookup: UuidLookup,
) -> Result<Vec<LoadedImage>>
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
    let mut images = vec![LoadedImage {
        path,
        load_address: executable.start,
        build_id: BuildIdentity::Unknown,
        role: ImageRole::MainExecutable,
    }];
    let dyld = infos.and_then(|infos| dyld_image(task, infos, &images)).or_else(|| {
        readable
            .find(|region| is_header_of_type(task, region.start, MH_DYLINKER))
            .map(|region| LoadedImage {
                path: PathBuf::from(DEFAULT_DYLD_PATH),
                load_address: region.start,
                build_id: BuildIdentity::Unknown,
                role: ImageRole::Library,
            })
    });
    images.extend(dyld);
    fill_uuids(task, &mut images, &uuids, cache, lookup, infos.and_then(image_list_timestamp));
    finish_image_list(images)
}

pub(crate) fn loaded_images(task: mach_port_t, pid: ProcessId) -> Result<Vec<LoadedImage>>
{
    // Birth-site setup asks once, then keeps the sites. A cache that dies with the call
    // still reads each missing UUID a single time.
    let mut cache = MappedUuidCache::default();
    loaded_images_with(task, pid, &mut cache, UuidLookup::Cache)
}

/// Enumerate images with exactly one marked main executable on success.
pub(crate) fn loaded_images_with(
    task: mach_port_t,
    pid: ProcessId,
    cache: &mut MappedUuidCache,
    lookup: UuidLookup,
) -> Result<Vec<LoadedImage>>
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
        return bootstrap_images(task, pid, None, cache, lookup);
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
        return bootstrap_images(task, pid, Some(&infos), cache, lookup);
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
    let uuids = image_uuids(task, &infos);
    let mut images = Vec::with_capacity(length);
    for record in records.as_chunks::<24>().0 {
        let load_address = word(record, 0);
        if load_address == 0 {
            return Err(invalid("null image load address"));
        }
        let path = remote_path(task, word(record, 8))?;
        images.push(LoadedImage {
            path,
            load_address: Address::new(load_address),
            build_id: BuildIdentity::Unknown,
            role: ImageRole::Library,
        });
    }
    // Mark the main image by its `MH_EXECUTE` header, so this holds after its file is
    // unlinked, and keep the main-first order callers see; dyld's order is not an API.
    if let Some(index) = images.iter().position(|image| is_executable_header(task, image.load_address)) {
        images[index].role = ImageRole::MainExecutable;
        images.swap(0, index);
    } else if let Some(first) = images.first_mut() {
        // No readable `MH_EXECUTE` header at this stop (for example its page was made
        // unreadable). dyld lists the main executable first, so keep resolving there
        // rather than report a process with no program.
        first.role = ImageRole::MainExecutable;
    }
    images.extend(dyld_image(task, &infos, &images));
    // UUID reads run before the stability check, as #170 required: a list that changes
    // while they run is rejected. A rejected list drops the cache updates from this call.
    fill_uuids(task, &mut images, &uuids, cache, lookup, image_list_timestamp(&infos));
    if read_target(task, address, 16)? != before {
        if lookup == UuidLookup::Cache {
            cache.clear();
        }
        return Err(invalid("image list changed while reading"));
    }
    finish_image_list(images)
}

fn finish_image_list(images: Vec<LoadedImage>) -> Result<Vec<LoadedImage>>
{
    let mut mains = images.iter().filter(|image| image.role == ImageRole::MainExecutable);
    let main = mains.next().ok_or_else(|| invalid("no main executable"))?;
    if let Some(second) = mains.next() {
        return Err(invalid(&format!(
            "more than one main executable ({} and {})",
            main.path.display(),
            second.path.display()
        )));
    }
    Ok(images)
}

thread_local! {
    static UUID_LOOKUP: Cell<UuidLookup> = const { Cell::new(UuidLookup::Cache) };
    static UUID_CACHE_EPOCH: Cell<u64> = const { Cell::new(0) };
    static MAPPED_UUID_HEADER_READS: Cell<u64> = const { Cell::new(0) };
}

fn note_mapped_uuid_header_read()
{
    MAPPED_UUID_HEADER_READS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

/// The lookup mode for `images()` calls on this thread. Production stays [`UuidLookup::Cache`].
pub(crate) fn current_uuid_lookup() -> UuidLookup
{
    UUID_LOOKUP.with(Cell::get)
}

/// Drop cached UUIDs on the next `images()` call when a test has bumped the epoch.
pub(crate) fn sync_test_epoch(cache: &mut MappedUuidCache)
{
    let epoch = UUID_CACHE_EPOCH.with(Cell::get);
    if cache.test_epoch != epoch {
        cache.clear();
        cache.test_epoch = epoch;
    }
}

/// Restores the previous mapped-UUID lookup mode when dropped.
///
/// Integration tests use this to time the pre-#170 and post-#170 costs. The mode is
/// thread-local, and libtest reuses threads, so it must not outlive the test section.
#[doc(hidden)]
#[must_use = "the lookup mode is restored when this guard drops"]
pub struct UuidLookupGuard
{
    previous: UuidLookup,
}

impl Drop for UuidLookupGuard
{
    fn drop(&mut self)
    {
        UUID_LOOKUP.with(|cell| cell.set(self.previous));
    }
}

fn force_uuid_lookup(lookup: UuidLookup) -> UuidLookupGuard
{
    UuidLookupGuard {
        previous: UUID_LOOKUP.with(|cell| cell.replace(lookup)),
    }
}

/// Skip mapped `LC_UUID` reads until the guard drops (the cost from before #170).
#[doc(hidden)]
#[must_use = "mapped UUID reads stay skipped until this guard drops"]
pub fn without_mapped_uuid_reads() -> UuidLookupGuard
{
    force_uuid_lookup(UuidLookup::Never)
}

/// Read every mapped `LC_UUID` and ignore the cache until the guard drops (the #170 cost).
#[doc(hidden)]
#[must_use = "mapped UUID reads stay uncached until this guard drops"]
pub fn reread_mapped_uuids() -> UuidLookupGuard
{
    force_uuid_lookup(UuidLookup::Always)
}

/// Forget cached UUIDs on the next `images()` call on this thread.
#[doc(hidden)]
pub fn invalidate_mapped_uuid_cache()
{
    UUID_CACHE_EPOCH.with(|cell| cell.set(cell.get().wrapping_add(1)));
}

/// Remote header reads performed by mapped `LC_UUID` lookup since the last reset, on
/// this thread.
#[doc(hidden)]
pub fn mapped_uuid_header_reads() -> u64
{
    MAPPED_UUID_HEADER_READS.with(Cell::get)
}

/// Zero [`mapped_uuid_header_reads`] and return the previous count.
#[doc(hidden)]
pub fn reset_mapped_uuid_header_reads() -> u64
{
    MAPPED_UUID_HEADER_READS.with(|cell| cell.replace(0))
}

#[cfg(test)]
mod tests
{
    use std::cell::{Cell, RefCell};

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

    #[test]
    fn image_list_timestamp_is_the_version_15_field_before_dyld_path()
    {
        let mut bytes = infos(15, ALL_IMAGE_INFOS_PREFIX, 0, 0);
        bytes[INFO_ARRAY_CHANGE_TIMESTAMP_OFFSET..INFO_ARRAY_CHANGE_TIMESTAMP_OFFSET + 8]
            .copy_from_slice(&0x1111_u64.to_le_bytes());
        assert_eq!(image_list_timestamp(&bytes), Some(0x1111));
        // Version 14 has no timestamp, whatever those bytes hold.
        assert_eq!(image_list_timestamp(&infos(14, ALL_IMAGE_INFOS_PREFIX, 0, 0)), None);
        assert_eq!(
            image_list_timestamp(&infos(15, INFO_ARRAY_CHANGE_TIMESTAMP_OFFSET + 7, 0, 0)),
            None
        );
        assert_eq!(image_list_timestamp(&[]), None);
    }

    fn listed(path: &str, address: u64) -> LoadedImage
    {
        LoadedImage {
            path: PathBuf::from(path),
            load_address: Address::new(address),
            build_id: BuildIdentity::Unknown,
            role: ImageRole::Library,
        }
    }

    #[test]
    fn two_marked_main_executables_are_reported()
    {
        let mut first = listed("/bin/first", 0x1000);
        first.role = ImageRole::MainExecutable;
        let mut second = listed("/bin/second", 0x3000);
        second.role = ImageRole::MainExecutable;
        let library = listed("/usr/lib/libSystem.B.dylib", 0x2000);
        let error = finish_image_list(vec![first, library, second]).expect_err("two main executables");
        assert!(matches!(
            error,
            DebuggerError::Os { code: -1, message }
            if message == "dyld image list: more than one main executable (/bin/first and /bin/second)"
        ));
    }

    #[test]
    fn a_missing_main_executable_is_reported()
    {
        let library = listed("/usr/lib/libSystem.B.dylib", 0x1000);
        for images in [vec![], vec![library]] {
            let error = finish_image_list(images).expect_err("no main executable");
            assert!(matches!(
                error,
                DebuggerError::Os { code: -1, message }
                if message == "dyld image list: no main executable"
            ));
        }
    }

    /// An unchanged list does no header reads. A different image at the same address,
    /// whether dyld's list generation moved or the path at that address changed, is read
    /// again. The sibling image that did not change is not.
    #[test]
    fn unchanged_image_list_skips_header_reads_and_a_replacement_is_reread()
    {
        let path_a = "/usr/lib/libSystem.B.dylib";
        let path_b = "/usr/lib/libc++.1.dylib";
        let mut images = vec![listed(path_a, 0x1000), listed(path_b, 0x2000)];
        // The closure outlives each assertion, so the log cannot be a plain `&mut Vec`.
        let reads = RefCell::new(Vec::new());
        let mut read = |address: u64| {
            reads.borrow_mut().push(address);
            let len = reads.borrow().len();
            MappedUuidRead::Parsed(Some([u8::try_from(len).expect("few reads"); 16]))
        };
        let mut cache = MappedUuidCache::default();
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(10), &mut read);
        assert_eq!(reads.borrow().as_slice(), &[0x1000, 0x2000]);
        let first = images.clone();

        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(10), &mut read);
        assert_eq!(
            reads.borrow().as_slice(),
            &[0x1000, 0x2000],
            "an unchanged list does not re-read known headers"
        );
        assert_eq!(images, first);

        // Same list generation, but a different file is now mapped at the first address.
        images[0].path = PathBuf::from("/usr/lib/libreplaced.dylib");
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(10), &mut read);
        assert_eq!(
            reads.borrow().as_slice(),
            &[0x1000, 0x2000, 0x1000],
            "a different path at the same address is read again"
        );
        assert_ne!(images[0].build_id, first[0].build_id);
        assert_eq!(
            images[1].build_id, first[1].build_id,
            "the image that stayed mapped is not read again"
        );

        // The loader reports the original path again, and bumps the list generation, which
        // is how a rebuilt image replaces one at the same address without a path change.
        images[0].path = PathBuf::from(path_a);
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(11), &mut read);
        assert_eq!(
            reads.borrow().as_slice(),
            &[0x1000, 0x2000, 0x1000, 0x1000, 0x2000],
            "a new image list re-reads the image at the same address"
        );
        assert_ne!(images[0].build_id, first[0].build_id);
    }

    #[test]
    fn a_failed_mapped_uuid_read_is_not_cached()
    {
        let mut images = vec![listed("/usr/lib/libdyld.dylib", 0x2000)];
        let reads = Cell::new(0_u32);
        // The first two lookups fail; the third parses. A later lookup must not read again.
        let mut read = |_| {
            let count = reads.get() + 1;
            reads.set(count);
            if count < 3 {
                MappedUuidRead::Failed
            } else {
                MappedUuidRead::Parsed(Some([9; 16]))
            }
        };
        let mut cache = MappedUuidCache::default();
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(1), &mut read);
        assert_eq!(images[0].build_id, BuildIdentity::Unknown);
        assert_eq!(reads.get(), 1);
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(1), &mut read);
        assert_eq!(reads.get(), 2, "a failed read is retried on the next list");
        assert_eq!(images[0].build_id, BuildIdentity::Unknown);
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(1), &mut read);
        assert_eq!(images[0].build_id, BuildIdentity::Known(BuildId::from([9; 16])));
        assert_eq!(reads.get(), 3);
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(1), &mut read);
        assert_eq!(reads.get(), 3, "the successful read is kept");
    }

    #[test]
    fn a_header_without_lc_uuid_is_cached_as_absent()
    {
        let mut images = vec![listed("/usr/lib/libcache.dylib", 0x3000)];
        let mut reads = 0;
        let mut read = |_| {
            reads += 1;
            MappedUuidRead::Parsed(None)
        };
        let mut cache = MappedUuidCache::default();
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(4), &mut read);
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, Some(4), &mut read);
        assert_eq!(images[0].build_id, BuildIdentity::Unknown);
        assert_eq!(reads, 1, "a parsed header with no LC_UUID is not read again");
    }

    #[test]
    fn a_dyld_uuid_is_used_without_reading_the_mapped_header()
    {
        let mut images = vec![listed("/usr/lib/libSystem.B.dylib", 0x1000)];
        let mut reads = 0;
        let mut read = |_| {
            reads += 1;
            MappedUuidRead::Failed
        };
        let dyld = HashMap::from([(0x1000, [7_u8; 16])]);
        let mut cache = MappedUuidCache::default();
        assign_cached_uuids(&mut images, &dyld, &mut cache, Some(1), &mut read);
        assert_eq!(reads, 0);
        assert_eq!(images[0].build_id, BuildIdentity::Known(BuildId::from([7; 16])));
    }

    /// Without `infoArrayChangeTimestamp`, the path and address list is the generation.
    #[test]
    fn an_untimestamped_list_change_rereads_a_replaced_image()
    {
        let mut images = vec![
            listed("/usr/lib/libSystem.B.dylib", 0x1000),
            listed("/usr/lib/libc++.1.dylib", 0x2000),
        ];
        let reads = RefCell::new(Vec::new());
        let mut read = |address: u64| {
            reads.borrow_mut().push(address);
            MappedUuidRead::Parsed(Some([1; 16]))
        };
        let mut cache = MappedUuidCache::default();
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, None, &mut read);
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, None, &mut read);
        assert_eq!(reads.borrow().as_slice(), &[0x1000, 0x2000]);
        images[0].path = PathBuf::from("/tmp/replaced.dylib");
        assign_cached_uuids(&mut images, &HashMap::new(), &mut cache, None, &mut read);
        assert_eq!(reads.borrow().as_slice(), &[0x1000, 0x2000, 0x1000, 0x2000]);
    }
}
