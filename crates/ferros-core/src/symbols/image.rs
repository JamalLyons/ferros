//! Binary image parsing and DWARF section loading.
//!
//! This module provides functionality to parse binary images (executables and
//! shared libraries) and load their DWARF debugging sections. It handles:
//!
//! - Binary format parsing (ELF, Mach-O, PE)
//! - DWARF section extraction (`.debug_info`, `.debug_line`, etc.)
//! - Address relocation (mapping runtime addresses to file addresses)
//! - Symbolication (mapping addresses to function names and source locations)
//! - Type information extraction
//!
//! ## Image Descriptors
//!
//! An `ImageDescriptor` describes a binary image that's mapped into the debuggee's
//! address space. It contains the path to the binary file and its load address
//! (the address where the binary is mapped at runtime).
//!
//! ## Address Relocation
//!
//! When a binary is loaded at runtime, it may be mapped at a different address
//! than its link-time address. The `BinaryImage` handles this relocation by
//! computing a "slide" (offset) between the file's virtual addresses and the
//! runtime addresses.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use addr2line::Context;
use gimli::{Dwarf, EndianArcSlice, RunTimeEndian, SectionId};
use object::macho::MachHeader64;
use object::read::macho::{MachHeader as _, Segment as _};
use object::{Endianness, Object, ObjectSection, ObjectSegment};
use once_cell::sync::OnceCell;

use super::cache::{SymbolFrame, Symbolication};
use super::debuginfo::{self, DebugInfoOptions};
use super::demangle::make_symbol_name;
use super::extractor::{TypeExtractor, TypeSummary};
use super::functions::FunctionIndex;
use super::lines::{LineIndex, SourceLookupError};
use super::{OwnedDwarf, OwnedReader};
use crate::error::{DebuggerError, Result};
use crate::types::{Address, Architecture, FunctionParameter, SourceLocation, SymbolSource};

/// Describes a binary image mapped in the debuggee's address space.
///
/// This structure contains the information needed to locate and parse a binary
/// image (executable or shared library) that's loaded in the target process.
///
/// ## Example
///
/// ```rust
/// use std::path::PathBuf;
///
/// use ferros_core::symbols::ImageDescriptor;
///
/// let descriptor = ImageDescriptor {
///     path: PathBuf::from("/usr/bin/my_program"),
///     load_address: 0x100000000, // Address where binary is loaded
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ImageDescriptor
{
    /// Path to the binary file on disk
    ///
    /// This should be an absolute path to the executable or shared library.
    pub path: PathBuf,
    /// Load address where the binary is mapped in the debuggee's address space
    ///
    /// This is the runtime address of the binary's header (reported by the loader),
    /// corresponding to file offset zero. Used to compute address relocation.
    pub load_address: u64,
}

/// Checked address translation for the executable's mapped segments.
///
/// This contains no DWARF and can therefore be used even for stripped images.
/// `load_address` names the runtime Mach-O header; the corresponding file virtual
/// address comes from the segment covering file offset zero, excluding `__PAGEZERO`.
///
/// [`parse`](Self::parse) reads the layout from the file on disk.
/// [`parse_slice`](Self::parse_slice) selects a slice from a universal file.
/// [`from_header`](Self::from_header) reads it from the header and load commands the
/// loader mapped, which stay valid while the file is replaced or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMapping
{
    file_base: u64,
    load_address: u64,
    segments: Vec<(u64, u64, u64)>,
}

impl ImageMapping
{
    /// Read an executable's segment layout without discovering debug information.
    ///
    /// Use [`parse_slice`](Self::parse_slice) to select a slice of a universal file
    /// on disk, or [`from_header`](Self::from_header) to read from target memory.
    ///
    /// # Errors
    /// Returns an error for unreadable/malformed files, universal files or overflowing
    /// mappings.
    pub fn parse(descriptor: &ImageDescriptor) -> Result<Self>
    {
        Self::parse_file(descriptor, None)
    }

    /// Read the segment layout of the selected Mach-O slice without discovering
    /// debug information (#175).
    ///
    /// # Errors
    /// Returns an error for unreadable or malformed files, a missing slice,
    /// a UUID mismatch, or overflowing mappings.
    pub fn parse_slice(descriptor: &ImageDescriptor, selector: &SliceIdentity) -> Result<Self>
    {
        Self::parse_file(descriptor, Some(selector))
    }

    fn parse_file(descriptor: &ImageDescriptor, selector: Option<&SliceIdentity>) -> Result<Self>
    {
        let bytes = fs::read(&descriptor.path)?;
        let data = match selector {
            Some(selector) => image_slice(&descriptor.path, &bytes, Some(selector))?,
            None if debuginfo::universal_slices(&bytes).is_some() => {
                return Err(DebuggerError::InvalidArgument(format!(
                    "{} is a universal file: which slice the target mapped is unknown; read the mapped header instead",
                    descriptor.path.display()
                )));
            }
            None => bytes.as_slice(),
        };
        let file =
            object::File::parse(data).map_err(|error| DebuggerError::InvalidArgument(format!("invalid image: {error}")))?;
        Self::from_file(&file, descriptor.load_address)
    }

    /// Read the segment layout from a 64-bit Mach-O header and its load commands, as
    /// mapped at `load_address` in the target. `header` must start at the header and
    /// hold at least [`MACH_HEADER_SIZE`](Self::MACH_HEADER_SIZE) plus its `sizeofcmds`
    /// bytes; see [`header_size`](Self::header_size).
    ///
    /// # Errors
    /// Returns an error for a truncated or malformed header or overflowing mappings.
    pub fn from_header(header: &[u8], load_address: u64) -> Result<Self>
    {
        let invalid = |error: object::Error| DebuggerError::InvalidArgument(format!("invalid image header: {error}"));
        let mach = MachHeader64::<Endianness>::parse(header, 0).map_err(invalid)?;
        let endian = mach.endian().map_err(invalid)?;
        let mut commands = mach.load_commands(endian, header, 0).map_err(invalid)?;
        let mut segments = Vec::new();
        while let Some(command) = commands.next().map_err(invalid)? {
            if let Some((segment, _)) = command.segment_64().map_err(invalid)? {
                segments.push(SegmentLayout {
                    is_page_zero: segment.name() == b"__PAGEZERO",
                    address: segment.vmaddr(endian),
                    size: segment.vmsize(endian),
                    file_offset: segment.fileoff(endian),
                    file_size: segment.filesize(endian),
                });
            }
        }
        Self::from_segments(&segments, load_address)
    }

    /// Size of a 64-bit Mach-O header, which precedes its load commands.
    pub const MACH_HEADER_SIZE: usize = 32;

    /// Largest `sizeofcmds` [`header_size`](Self::header_size) accepts.
    const MAX_LOAD_COMMANDS_SIZE: usize = 1 << 20;

    /// Bytes [`from_header`](Self::from_header) needs, given the first
    /// [`MACH_HEADER_SIZE`](Self::MACH_HEADER_SIZE) bytes at the load address.
    ///
    /// # Errors
    /// Returns an error unless `header` starts a 64-bit Mach-O header.
    pub fn header_size(header: &[u8]) -> Result<usize>
    {
        let mach = MachHeader64::<Endianness>::parse(header, 0)
            .map_err(|error| DebuggerError::InvalidArgument(format!("invalid image header: {error}")))?;
        let endian = mach
            .endian()
            .map_err(|error| DebuggerError::InvalidArgument(format!("invalid image header: {error}")))?;
        // The size is target-controlled: bound the read before anyone allocates it.
        usize::try_from(mach.sizeofcmds(endian))
            .ok()
            .filter(|&commands| commands <= Self::MAX_LOAD_COMMANDS_SIZE)
            .map(|commands| commands + Self::MACH_HEADER_SIZE)
            .ok_or_else(|| DebuggerError::InvalidArgument("image load commands exceed the size limit".into()))
    }

    fn from_file(file: &object::File<'_>, load_address: u64) -> Result<Self>
    {
        let segments: Vec<_> = file
            .segments()
            .map(|segment| SegmentLayout {
                is_page_zero: segment.name().ok().flatten() == Some("__PAGEZERO"),
                address: segment.address(),
                size: segment.size(),
                file_offset: segment.file_range().0,
                file_size: segment.file_range().1,
            })
            .collect();
        Self::from_segments(&segments, load_address)
    }

    fn from_segments(segments: &[SegmentLayout], load_address: u64) -> Result<Self>
    {
        let file_base = segments
            .iter()
            .find(|segment| segment.file_offset == 0 && segment.file_size != 0)
            .map(|segment| segment.address)
            .ok_or_else(|| DebuggerError::InvalidArgument("image has no segment covering its header".into()))?;
        let mut mapping = Self {
            file_base,
            load_address,
            segments: Vec::new(),
        };
        for segment in segments {
            if segment.is_page_zero || segment.size == 0 {
                continue;
            }
            let address = segment.address;
            let runtime = mapping
                .relocate(address)
                .ok_or_else(|| DebuggerError::InvalidArgument("image relocation overflows address space".into()))?;
            address
                .checked_add(segment.size)
                .and_then(|_| runtime.checked_add(segment.size))
                .ok_or_else(|| DebuggerError::InvalidArgument("image segment overflows address space".into()))?;
            mapping.segments.push((address, runtime, segment.size));
        }
        Ok(mapping)
    }

    fn relocate(&self, address: u64) -> Option<u64>
    {
        u64::try_from(i128::from(address) + i128::from(self.load_address) - i128::from(self.file_base)).ok()
    }

    fn runtime_range(&self) -> (u64, u64)
    {
        (
            self.load_address,
            self.segments
                .iter()
                .map(|&(_, start, size)| start + size)
                .max()
                .unwrap_or(self.load_address),
        )
    }

    /// Whether the runtime address lies in an actual mapped segment.
    pub fn contains(&self, address: Address) -> bool
    {
        self.file_address(address).is_some()
    }

    /// Convert a runtime address to a file virtual address, excluding segment gaps.
    pub fn file_address(&self, address: Address) -> Option<u64>
    {
        self.segments.iter().find_map(|&(file, runtime, size)| {
            address
                .value()
                .checked_sub(runtime)
                .filter(|offset| *offset < size)
                .and_then(|offset| file.checked_add(offset))
        })
    }

    /// Convert a file virtual address to a runtime address, excluding segment gaps.
    pub fn runtime_address(&self, address: u64) -> Option<Address>
    {
        self.segments.iter().find_map(|&(file, runtime, size)| {
            address
                .checked_sub(file)
                .filter(|offset| *offset < size)
                .and_then(|offset| runtime.checked_add(offset))
                .map(Address::new)
        })
    }
}

/// One segment load command, from a file or from mapped load commands.
struct SegmentLayout
{
    is_page_zero: bool,
    address: u64,
    size: u64,
    file_offset: u64,
    file_size: u64,
}

/// Selects a slice of a universal (fat) Mach-O file by its CPU type, CPU subtype,
/// and optional `LC_UUID` (#175).
///
/// CPU subtype capability bits are ignored. When the UUID is omitted, the first
/// readable slice of the requested CPU type and subtype is selected. For a
/// single-architecture file, only the supplied UUID is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceIdentity
{
    /// `cputype` of the mapped header.
    pub(crate) cpu_type: u32,
    /// `cpusubtype` of the mapped header, capability bits included.
    pub(crate) cpu_subtype: u32,
    /// `LC_UUID` of the mapped header, when it has one.
    pub(crate) uuid: Option<[u8; 16]>,
}

impl SliceIdentity
{
    /// Create a selector from Mach-O `cputype`, `cpusubtype`, and an optional `LC_UUID`
    /// (#175). The UUID distinguishes builds of the same CPU type and subtype.
    ///
    /// ```rust
    /// use ferros_core::symbols::SliceIdentity;
    ///
    /// let arm64e = SliceIdentity::new(0x0100_000c, 2, None);
    /// ```
    #[must_use]
    pub const fn new(cpu_type: u32, cpu_subtype: u32, uuid: Option<[u8; 16]>) -> Self
    {
        Self {
            cpu_type,
            cpu_subtype,
            uuid,
        }
    }

    /// Read the identity from a 64-bit Mach-O header and its load commands, as
    /// [`ImageMapping::from_header`] takes them.
    ///
    /// # Errors
    /// Returns an error for a truncated or malformed header.
    pub(crate) fn from_header(header: &[u8]) -> Result<Self>
    {
        let invalid = |error: object::Error| DebuggerError::InvalidArgument(format!("invalid image header: {error}"));
        let mach = MachHeader64::<Endianness>::parse(header, 0).map_err(invalid)?;
        let endian = mach.endian().map_err(invalid)?;
        let mut commands = mach.load_commands(endian, header, 0).map_err(invalid)?;
        let mut uuid = None;
        while let Some(command) = commands.next().map_err(invalid)? {
            if let Some(command) = command.uuid().map_err(invalid)? {
                uuid = Some(command.uuid);
                break;
            }
        }
        Ok(Self {
            cpu_type: mach.cputype(endian),
            cpu_subtype: mach.cpusubtype(endian),
            uuid,
        })
    }

    /// Whether a fat header entry's CPU type and subtype name this slice. Capability
    /// bits (`CPU_SUBTYPE_MASK`, such as arm64e's pointer-authentication ABI) are not
    /// part of the subtype.
    fn names(&self, cpu_type: u32, cpu_subtype: u32) -> bool
    {
        cpu_type == self.cpu_type && base_subtype(cpu_subtype) == base_subtype(self.cpu_subtype)
    }
}

fn base_subtype(cpu_subtype: u32) -> u32
{
    cpu_subtype & !object::macho::CPU_SUBTYPE_MASK
}

/// The Mach-O bytes of the image at `path` in its file `bytes`: the file itself, or for a
/// universal file the slice the target mapped (#169). A slice must match `target`'s CPU
/// type and subtype and, when `target` knows it, its UUID. A single-architecture file
/// must match that UUID too, so a file rebuilt while the old build stays mapped is never
/// parsed as the mapped build (#170).
///
/// # Errors
/// [`DebuggerError::NoDebugInfo`] for a universal file with no slice of `target`'s CPU
/// type and subtype, or when `target` is unknown so no slice can be chosen.
/// [`DebuggerError::InvalidArgument`] for a malformed universal file, when every slice
/// of that CPU is damaged or another build, or when a single-architecture file is another
/// build than the mapped one (the file was replaced): a failure the session retries once
/// the file changes, not missing debug info it keeps.
pub(crate) fn image_slice<'data>(path: &Path, bytes: &'data [u8], target: Option<&SliceIdentity>) -> Result<&'data [u8]>
{
    let Some(slices) = debuginfo::universal_slices(bytes) else {
        return single_image(path, bytes, target);
    };
    let slices = slices
        .map_err(|error| DebuggerError::InvalidArgument(format!("invalid universal file {}: {error}", path.display())))?;
    let no_slice = |detail: String| DebuggerError::NoDebugInfo {
        path: path.to_path_buf(),
        detail,
    };
    let Some(target) = target else {
        return Err(no_slice("universal file, and the target's architecture is unknown".into()));
    };
    // The first slice whose CPU type and subtype match and whose UUID matches when the
    // target knows it. Why a candidate was rejected is kept: a damaged slice is reported
    // as such, and a slice of another build as a replaced file, not as missing debug info.
    let mut rejected = None;
    for slice in slices.iter().filter(|slice| target.names(slice.cpu_type, slice.cpu_subtype)) {
        let data = match slice.data {
            Ok(data) => data,
            Err(error) => {
                rejected.get_or_insert_with(|| format!("slice range: {error}"));
                continue;
            }
        };
        let Some(uuid) = target.uuid else {
            return Ok(data);
        };
        match object::File::parse(data).and_then(|file| file.mach_uuid()) {
            Ok(Some(found)) if found == uuid => return Ok(data),
            Ok(found) => {
                rejected.get_or_insert_with(|| {
                    format!(
                        "slice is another build (UUID {}) than the mapped image",
                        found.map_or_else(|| "none".into(), |found| hex(&found))
                    )
                });
            }
            Err(error) => {
                rejected.get_or_insert_with(|| format!("slice: {error}"));
            }
        }
    }
    let wanted = format!(
        "cputype {:#x} cpusubtype {:#x}{}",
        target.cpu_type,
        target.cpu_subtype,
        target
            .uuid
            .map_or_else(String::new, |uuid| format!(" with UUID {}", hex(&uuid)))
    );
    Err(match rejected {
        Some(detail) => DebuggerError::InvalidArgument(format!(
            "invalid universal file {}: {detail}, and no other slice for {wanted}",
            path.display()
        )),
        None => no_slice(format!("universal file has no slice for {wanted}")),
    })
}

/// A single-architecture file is read as it is, unless `target` knows the mapped UUID and
/// the file has another (or none): then it is another build than the mapped one (#170).
/// A file that does not parse is left to the caller's parse, which reports it.
fn single_image<'data>(path: &Path, bytes: &'data [u8], target: Option<&SliceIdentity>) -> Result<&'data [u8]>
{
    let Some(uuid) = target.and_then(|target| target.uuid) else {
        return Ok(bytes);
    };
    let Ok(found) = object::File::parse(bytes).and_then(|file| file.mach_uuid()) else {
        return Ok(bytes);
    };
    if found == Some(uuid) {
        return Ok(bytes);
    }
    Err(DebuggerError::InvalidArgument(format!(
        "{} is another build (UUID {}) than the mapped image (UUID {}): the file was replaced",
        path.display(),
        found.map_or_else(|| "none".into(), |found| hex(&found)),
        hex(&uuid)
    )))
}

fn hex(bytes: &[u8]) -> String
{
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Unique identifier for a binary image.
///
/// The symbol cache keys on path and load address. When a slice is selected,
/// its CPU type, subtype, and UUID (if supplied) also contribute to the key
/// (#170, #175).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId(u64);

impl ImageId
{
    /// Create an `ImageId` from a path and load address.
    ///
    /// The ID is computed by hashing the path and load address together. This
    /// ensures that the same binary loaded at different addresses gets different IDs.
    ///
    /// ## Parameters
    ///
    /// - `path`: Path to the binary file
    /// - `load_address`: Load address where the binary is mapped
    ///
    /// ## Example
    ///
    /// ```rust
    /// use std::path::Path;
    ///
    /// use ferros_core::symbols::ImageId;
    ///
    /// let id1 = ImageId::from_parts(Path::new("/usr/bin/prog"), 0x100000000);
    /// let id2 = ImageId::from_parts(Path::new("/usr/bin/prog"), 0x200000000);
    /// assert_ne!(id1, id2); // Different load addresses = different IDs
    /// ```
    pub fn from_parts(path: &Path, load_address: u64) -> Self
    {
        Self::with_slice(path, load_address, None)
    }

    pub(crate) fn with_slice(path: &Path, load_address: u64, selector: Option<&SliceIdentity>) -> Self
    {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        load_address.hash(&mut hasher);
        if let Some(selector) = selector {
            (selector.cpu_type, base_subtype(selector.cpu_subtype), selector.uuid).hash(&mut hasher);
        }
        ImageId(hasher.finish())
    }

    /// Get the raw `u64` value of this image ID.
    ///
    /// This is useful for serialization or debugging. The value hashes the path,
    /// load address, and any slice selector.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use std::path::Path;
    ///
    /// use ferros_core::symbols::ImageId;
    ///
    /// let id = ImageId::from_parts(Path::new("/usr/bin/prog"), 0x100000000);
    /// let raw = id.as_u64();
    /// ```
    pub fn as_u64(self) -> u64
    {
        self.0
    }
}

#[derive(Clone)]
struct SectionBlob
{
    data: Arc<[u8]>,
    address: u64,
}

const DWARF_SECTIONS: &[(&str, &[&str])] = &[
    (".debug_abbrev", &[".debug_abbrev", "__debug_abbrev"]),
    (".debug_addr", &[".debug_addr", "__debug_addr"]),
    (".debug_info", &[".debug_info", "__debug_info"]),
    (".debug_line", &[".debug_line", "__debug_line"]),
    (".debug_line_str", &[".debug_line_str", "__debug_line_str"]),
    (".debug_ranges", &[".debug_ranges", "__debug_ranges"]),
    (".debug_rnglists", &[".debug_rnglists", "__debug_rnglists"]),
    (".debug_str", &[".debug_str", "__debug_str"]),
    (".debug_str_offsets", &[".debug_str_offsets", "__debug_str_offsets"]),
    (".debug_types", &[".debug_types", "__debug_types"]),
    (".debug_loc", &[".debug_loc", "__debug_loc"]),
    (".debug_loclists", &[".debug_loclists", "__debug_loclists"]),
    (".debug_pubnames", &[".debug_pubnames", "__debug_pubnames"]),
    (".debug_pubtypes", &[".debug_pubtypes", "__debug_pubtypes"]),
    (".debug_frame", &[".debug_frame", "__debug_frame"]),
    (".debug_macro", &[".debug_macro", "__debug_macro"]),
    (".debug_names", &[".debug_names", "__debug_names"]),
    (".debug_cu_index", &[".debug_cu_index"]),
    (".debug_tu_index", &[".debug_tu_index"]),
    (".debug_sup", &[".debug_sup"]),
    (".debug_str_sup", &[".debug_str_sup"]),
];

fn load_section_bytes<'data>(file: &object::File<'data>, names: &[&str]) -> Result<Arc<[u8]>>
{
    for name in names {
        if let Some(section) = file.section_by_name(name) {
            let data = section
                .uncompressed_data()
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to read {name}: {err}")))?;
            return Ok(match data {
                Cow::Borrowed(bytes) => Arc::<[u8]>::from(bytes.to_vec()),
                Cow::Owned(vec) => vec.into(),
            });
        }
    }

    Ok(Arc::<[u8]>::from(Vec::new()))
}

fn load_section_blob<'data>(file: &object::File<'data>, names: &[&str]) -> Result<Option<SectionBlob>>
{
    for name in names {
        if let Some(section) = file.section_by_name(name) {
            let address = section.address();
            let data = section
                .uncompressed_data()
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to read {name}: {err}")))?;
            let data = match data {
                Cow::Borrowed(bytes) => Arc::<[u8]>::from(bytes.to_vec()),
                Cow::Owned(vec) => vec.into(),
            };
            return Ok(Some(SectionBlob { data, address }));
        }
    }

    Ok(None)
}

/// Cached binary image with DWARF + unwind metadata.
///
/// This structure represents a parsed binary image (executable or shared library)
/// with all its DWARF debugging sections loaded and cached. It provides methods
/// for:
///
/// - Address symbolication (mapping addresses to function names and source locations)
/// - Type information extraction
/// - Unwind information access (`.eh_frame`, `.debug_frame`)
///
/// ## Caching
///
/// The `BinaryImage` caches parsed DWARF data and symbolication contexts to avoid
/// re-parsing on every lookup. The caches are lazily initialized on first use.
///
/// ## Address Relocation
///
/// The image handles address relocation between file addresses (link-time addresses)
/// and runtime addresses (where the binary is actually loaded). Use `file_address()`
/// to convert a runtime address to a file address for DWARF lookups.
pub struct BinaryImage
{
    id: ImageId,
    path: PathBuf,
    architecture: Architecture,
    endian: RunTimeEndian,
    mapping: ImageMapping,
    debug_sections: HashMap<&'static str, Arc<[u8]>>,
    eh_frame: Option<SectionBlob>,
    eh_frame_hdr: Option<SectionBlob>,
    debug_frame: Option<SectionBlob>,
    /// `__TEXT,__unwind_info` (Mach-O compact unwind) of the executable.
    unwind_info: Option<SectionBlob>,
    dwarf_cache: OnceCell<OwnedDwarf>,
    /// `addr2line::Context` is not `Sync`; the mutex makes `BinaryImage` `Send + Sync`
    /// so a `SymbolCache` (and the `Session` holding it) can move between threads.
    context_cache: OnceCell<Mutex<Context<OwnedReader>>>,
    type_cache: RwLock<HashMap<String, Arc<TypeSummary>>>,
    /// Built on the first source lookup (see `symbols::lines`); a failure to load the
    /// DWARF sections is cached too.
    line_index: OnceCell<std::result::Result<LineIndex, SourceLookupError>>,
    /// Built on the first function lookup (see `symbols::functions`); a failure to load the
    /// DWARF sections is cached too.
    function_index: OnceCell<std::result::Result<FunctionIndex, SourceLookupError>>,
}

impl BinaryImage
{
    /// Parse the image `desc` names. For a universal file, the slice `target` identifies
    /// is parsed ([`image_slice`]); without one, a universal file has no usable debug info.
    pub(crate) fn parse(desc: ImageDescriptor, target: Option<&SliceIdentity>, options: &DebugInfoOptions) -> Result<Self>
    {
        let bytes = fs::read(&desc.path)?;
        let data = image_slice(&desc.path, &bytes, target)?;
        let file = object::File::parse(data)
            .map_err(|err| DebuggerError::InvalidArgument(format!("failed to parse {}: {err}", desc.path.display())))?;

        let endian = if file.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };

        let architecture = match file.architecture() {
            object::Architecture::Aarch64 => Architecture::Arm64,
            object::Architecture::X86_64 => Architecture::X86_64,
            _ => Architecture::Unknown("unknown"),
        };

        let mapping = ImageMapping::from_file(&file, desc.load_address)?;
        let debug_bytes = debuginfo::discover(&desc.path, data, options)?;
        let debug_file = object::File::parse(&*debug_bytes)
            .map_err(|err| DebuggerError::InvalidArgument(format!("failed to parse DWARF: {err}")))?;

        let mut sections = HashMap::new();
        for (canonical, aliases) in DWARF_SECTIONS {
            sections.insert(*canonical, load_section_bytes(&debug_file, aliases)?);
        }

        let eh_frame = load_section_blob(&file, &[".eh_frame", "__eh_frame"])?;
        let eh_frame_hdr = load_section_blob(&file, &[".eh_frame_hdr", "__eh_frame_hdr"])?;
        let debug_frame = load_section_blob(&debug_file, &[".debug_frame", "__debug_frame"])?;
        let unwind_info = load_section_blob(&file, &["__unwind_info"])?;

        Ok(Self {
            id: ImageId::with_slice(&desc.path, desc.load_address, target),
            path: desc.path,
            architecture,
            endian,
            mapping,
            debug_sections: sections,
            eh_frame,
            eh_frame_hdr,
            debug_frame,
            unwind_info,
            dwarf_cache: OnceCell::new(),
            context_cache: OnceCell::new(),
            type_cache: RwLock::new(HashMap::new()),
            line_index: OnceCell::new(),
            function_index: OnceCell::new(),
        })
    }

    pub(crate) fn runtime_range(&self) -> (u64, u64)
    {
        self.mapping.runtime_range()
    }

    /// Get the pointer size in bytes for this image's architecture.
    ///
    /// Returns 8 for 64-bit architectures (ARM64, x86-64), or 8 for unknown
    /// architectures (defaults to 64-bit).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    /// use ferros_core::types::Address;
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// assert_eq!(image.pointer_size(), 8); // 64-bit
    /// ```
    pub fn pointer_size(&self) -> u8
    {
        self.architecture.pointer_size_bytes()
    }

    pub(crate) fn endian(&self) -> RunTimeEndian
    {
        self.endian
    }

    pub(crate) fn relocated_address(&self, vmaddr: u64) -> u64
    {
        self.mapping.relocate(vmaddr).unwrap_or(0)
    }

    pub(crate) fn eh_frame_section(&self) -> Option<(u64, &[u8])>
    {
        self.eh_frame.as_ref().map(|blob| (blob.address, blob.data.as_ref()))
    }

    pub(crate) fn eh_frame_hdr_section(&self) -> Option<(u64, &[u8])>
    {
        self.eh_frame_hdr.as_ref().map(|blob| (blob.address, blob.data.as_ref()))
    }

    pub(crate) fn debug_frame_section(&self) -> Option<(u64, &[u8])>
    {
        self.debug_frame.as_ref().map(|blob| (blob.address, blob.data.as_ref()))
    }

    pub(crate) fn unwind_info_section(&self) -> Option<&[u8]>
    {
        self.unwind_info.as_ref().map(|blob| blob.data.as_ref())
    }

    /// Get the unique identifier for this image.
    ///
    /// The ID is computed from the image's path, load address, and any slice selector.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let id = image.id();
    /// ```
    pub fn id(&self) -> ImageId
    {
        self.id
    }

    /// Get the path to the binary file.
    ///
    /// Returns the absolute path to the binary file on disk.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let path = image.path();
    /// ```
    pub fn path(&self) -> &Path
    {
        &self.path
    }

    /// Get the CPU architecture of this image.
    ///
    /// Returns `Architecture::Arm64` for ARM64 binaries, `Architecture::X86_64`
    /// for x86-64 binaries, or `Architecture::Unknown` for other architectures.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    /// use ferros_core::types::Architecture;
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// match image.architecture() {
    ///     Architecture::Arm64 => println!("ARM64 binary"),
    ///     Architecture::X86_64 => println!("x86-64 binary"),
    ///     _ => println!("Unknown architecture"),
    /// }
    /// ```
    pub fn architecture(&self) -> Architecture
    {
        self.architecture
    }

    /// Check if an address is within this image's address range.
    ///
    /// Returns `true` if the address falls within the image's runtime address
    /// range (from `load_address` to `load_address + size`).
    ///
    /// ## Parameters
    ///
    /// - `address`: The runtime address to check
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    /// use ferros_core::types::Address;
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let addr = Address::from(0x100001000);
    /// if image.contains(addr) {
    ///     println!("Address is in this image");
    /// }
    /// ```
    pub fn contains(&self, address: Address) -> bool
    {
        self.mapping.contains(address)
    }

    /// Convert a runtime address to a file address (link-time address).
    ///
    /// This method handles address relocation by subtracting the "slide" (offset
    /// between runtime and file addresses). Returns `None` if the address is not
    /// within this image's address range.
    ///
    /// ## Parameters
    ///
    /// - `address`: The runtime address to convert
    ///
    /// ## Returns
    ///
    /// `Some(file_address)` if the address is within this image, `None` otherwise.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    /// use ferros_core::types::Address;
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let runtime_addr = Address::from(0x100001000);
    /// if let Some(file_addr) = image.file_address(runtime_addr) {
    ///     // Use file_addr for DWARF lookups
    /// }
    /// ```
    pub fn file_address(&self, address: Address) -> Option<u64>
    {
        self.mapping.file_address(address)
    }

    /// Convert a file virtual address within a mapped segment to a runtime address.
    pub fn runtime_address(&self, address: u64) -> Option<Address>
    {
        self.mapping.runtime_address(address)
    }

    /// The lazily built line-table index slot, filled once by `symbols::lines` with the
    /// index or with the error that prevented building it.
    pub(super) fn line_index_cell(&self) -> &OnceCell<std::result::Result<LineIndex, SourceLookupError>>
    {
        &self.line_index
    }

    /// The lazily built function index slot, filled once by `symbols::functions` with the
    /// index or with the error that prevented building it.
    pub(super) fn function_index_cell(&self) -> &OnceCell<std::result::Result<FunctionIndex, SourceLookupError>>
    {
        &self.function_index
    }

    pub(super) fn dwarf(&self) -> Result<&OwnedDwarf>
    {
        self.dwarf_cache.get_or_try_init(|| {
            Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(section)))
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to load DWARF: {err}")))
        })
    }

    fn section_reader(&self, id: SectionId) -> OwnedReader
    {
        let key = match id {
            SectionId::DebugAbbrev => ".debug_abbrev",
            SectionId::DebugAddr => ".debug_addr",
            SectionId::DebugInfo => ".debug_info",
            SectionId::DebugLine => ".debug_line",
            SectionId::DebugLineStr => ".debug_line_str",
            SectionId::DebugRanges => ".debug_ranges",
            SectionId::DebugRngLists => ".debug_rnglists",
            SectionId::DebugStr => ".debug_str",
            SectionId::DebugStrOffsets => ".debug_str_offsets",
            SectionId::DebugTypes => ".debug_types",
            SectionId::DebugLoc => ".debug_loc",
            SectionId::DebugLocLists => ".debug_loclists",
            SectionId::DebugPubNames => ".debug_pubnames",
            SectionId::DebugPubTypes => ".debug_pubtypes",
            SectionId::DebugFrame => ".debug_frame",
            SectionId::DebugMacro => ".debug_macro",
            SectionId::DebugCuIndex => ".debug_cu_index",
            SectionId::DebugTuIndex => ".debug_tu_index",
            _ => "",
        };

        let data = self
            .debug_sections
            .get(key)
            .cloned()
            .unwrap_or_else(|| Arc::<[u8]>::from(Vec::new()));
        EndianArcSlice::new(data, self.endian)
    }

    fn symbol_context(&self) -> Result<&Mutex<Context<OwnedReader>>>
    {
        self.context_cache.get_or_try_init(|| {
            let dwarf = Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(section)))
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to load DWARF for addr2line: {err}")))?;
            Context::from_dwarf(dwarf)
                .map(Mutex::new)
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to build addr2line context: {err}")))
        })
    }

    /// Symbolicate an address to function names and source locations.
    ///
    /// This method maps a runtime address to its corresponding function name
    /// and source location using DWARF line information. It handles inlined
    /// functions by returning multiple frames (outermost to innermost).
    ///
    /// ## Parameters
    ///
    /// - `address`: The runtime address to symbolicate
    ///
    /// ## Returns
    ///
    /// `Some(symbolication)` if the address is found in this image, `None` otherwise.
    /// The symbolication contains multiple frames for inlined functions.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    /// use ferros_core::types::Address;
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let addr = Address::from(0x100001000);
    /// if let Some(symbolication) = image.symbolicate(addr) {
    ///     for frame in symbolication.frames {
    ///         println!("Function: {}", frame.symbol.display_name());
    ///         if let Some(loc) = frame.location {
    ///             println!("Location: {}:{}", loc.file, loc.line.unwrap_or(0));
    ///         }
    ///     }
    /// }
    /// ```
    pub fn symbolicate(&self, address: Address) -> Option<Symbolication>
    {
        let file_addr = match self.file_address(address) {
            Some(addr) => addr,
            None => {
                use tracing::debug;
                debug!("Address 0x{:x} not in image {}", address.value(), self.path.display());
                return None;
            }
        };

        let ctx = match self.symbol_context() {
            Ok(ctx) => ctx.lock().unwrap_or_else(PoisonError::into_inner),
            Err(e) => {
                use tracing::debug;
                debug!("Failed to get symbol context for {}: {}", self.path.display(), e);
                return None;
            }
        };

        let mut frames = Vec::new();

        let lookup = ctx.find_frames(file_addr);
        let mut frame_iter = match lookup.skip_all_loads() {
            Ok(iter) => iter,
            Err(e) => {
                use tracing::debug;
                debug!(
                    "Failed to find frames for 0x{:x} in {}: {:?}",
                    file_addr,
                    self.path.display(),
                    e
                );
                return None;
            }
        };

        while let Ok(Some(frame)) = frame_iter.next() {
            let symbol_name = frame
                .function
                .as_ref()
                .and_then(|func| func.raw_name().ok())
                .map(|raw| make_symbol_name(raw.to_string(), SymbolSource::DebugInfo));
            let location = frame.location.as_ref().and_then(|loc| {
                loc.file.as_ref().map(|file| SourceLocation {
                    file: file.to_string(),
                    line: loc.line,
                    column: loc.column,
                })
            });

            if let Some(symbol) = symbol_name {
                // Extract function parameters from DWARF
                let parameters = self.extract_function_parameters(file_addr, &frame);
                frames.push(SymbolFrame {
                    symbol,
                    location,
                    parameters,
                });
            } else {
                use tracing::debug;
                debug!("Frame at 0x{:x} has no symbol name", file_addr);
            }
        }

        if frames.is_empty() {
            use tracing::debug;
            debug!("No frames found for address 0x{:x} in {}", file_addr, self.path.display());
            return None;
        }

        Some(Symbolication {
            image_id: self.id,
            frames,
        })
    }

    /// Extract function parameters from DWARF for a given address and frame.
    ///
    /// This method walks the DWARF DIEs to find function parameters.
    fn extract_function_parameters(&self, file_addr: u64, _frame: &addr2line::Frame<OwnedReader>) -> Vec<FunctionParameter>
    {
        // TODO: Implement full DWARF parameter extraction
        // For now, return empty vector as parameter extraction from DWARF
        // requires walking the DIE tree and finding DW_TAG_formal_parameter entries
        // This is a placeholder that can be enhanced later
        let _ = file_addr;
        Vec::new()
    }

    /// Describe a type by name using DWARF type information.
    ///
    /// This method searches through the image's DWARF type information to find
    /// a type definition matching the given name. It returns a summary of the
    /// type's structure (fields, variants, etc.).
    ///
    /// ## Parameters
    ///
    /// - `name`: The type name to look up (e.g., "std::option::Option", "Point")
    ///
    /// ## Returns
    ///
    /// `Ok(Some(summary))` if the type is found, `Ok(None)` if not found,
    /// or an error if DWARF parsing fails.
    ///
    /// ## Caching
    ///
    /// Type summaries are cached after first lookup to avoid re-parsing DWARF
    /// on subsequent requests for the same type.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    ///
    /// let mut cache = SymbolCache::new();
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// if let Ok(Some(summary)) = image.describe_type("std::option::Option") {
    ///     println!("Type: {}", summary.name);
    ///     println!("Kind: {:?}", summary.kind);
    ///     for field in &summary.fields {
    ///         println!("Field: {:?}", field.name);
    ///     }
    /// }
    /// ```
    pub fn describe_type(&self, name: &str) -> Result<Option<Arc<TypeSummary>>>
    {
        if let Some(existing) = self.type_cache.read().unwrap().get(name) {
            return Ok(Some(existing.clone()));
        }

        let dwarf = self.dwarf()?;
        let extractor = TypeExtractor::new(dwarf)?;
        let Some(summary) = extractor.describe(name)? else {
            return Ok(None);
        };
        let summary = Arc::new(summary);
        let mut cache = self.type_cache.write().unwrap();
        cache.insert(name.to_string(), summary.clone());
        if summary.name != name {
            cache.insert(summary.name.clone(), summary.clone());
        }
        Ok(Some(summary))
    }
}

#[cfg(test)]
mod mapping_tests
{
    use super::*;

    fn macho_segments() -> Vec<u8>
    {
        let segments = [
            ("__PAGEZERO", 0_u64, 0x1_0000_0000_u64, 0_u64, 0_u64),
            ("__HEADER", 0x1_0000_0000, 0x1000, 0, 248),
            ("__DATA", 0x1_0000_2000, 0x1000, 0x1000, 0),
        ];
        let mut bytes = vec![0; 32 + 72 * 3];
        bytes[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&2_u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&3_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&216_u32.to_le_bytes());
        for (index, (name, address, size, file_offset, file_size)) in segments.into_iter().enumerate() {
            let command = &mut bytes[32 + 72 * index..32 + 72 * (index + 1)];
            command[0..4].copy_from_slice(&0x19_u32.to_le_bytes());
            command[4..8].copy_from_slice(&72_u32.to_le_bytes());
            command[8..8 + name.len()].copy_from_slice(name.as_bytes());
            command[24..32].copy_from_slice(&address.to_le_bytes());
            command[32..40].copy_from_slice(&size.to_le_bytes());
            command[40..48].copy_from_slice(&file_offset.to_le_bytes());
            command[48..56].copy_from_slice(&file_size.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn mapping_uses_header_segment_excludes_pagezero_and_rejects_overflow()
    {
        let bytes = macho_segments();
        let file = object::File::parse(&*bytes).unwrap();
        let mapping = ImageMapping::from_file(&file, 0x1000).unwrap();
        assert_eq!(mapping.file_address(Address::new(0x1020)), Some(0x1_0000_0020));
        assert_eq!(mapping.runtime_address(0x1_0000_2020), Some(Address::new(0x3020)));
        assert_eq!(mapping.runtime_address(0), None);
        assert_eq!(mapping.runtime_address(0x1_0000_1000), None);
        assert!(ImageMapping::from_file(&file, u64::MAX - 0x100).is_err());
    }

    #[test]
    fn mapped_header_gives_the_file_layout_and_rejects_bad_headers()
    {
        let bytes = macho_segments();
        let file = object::File::parse(&*bytes).unwrap();
        assert_eq!(ImageMapping::header_size(&bytes).unwrap(), bytes.len());
        assert_eq!(
            ImageMapping::from_header(&bytes, 0x1000).unwrap(),
            ImageMapping::from_file(&file, 0x1000).unwrap()
        );
        assert!(ImageMapping::from_header(&bytes[..bytes.len() - 1], 0x1000).is_err());
        assert!(ImageMapping::from_header(&bytes, u64::MAX - 0x100).is_err());
        assert!(ImageMapping::header_size(&[0; 32]).is_err());
        assert!(ImageMapping::header_size(&bytes[..16]).is_err());
        let mut huge = bytes.clone();
        huge[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ImageMapping::header_size(&huge).is_err());
    }

    #[test]
    fn translations_respect_each_segment_and_gaps_in_both_directions()
    {
        let mapping = ImageMapping {
            file_base: 0x1_0000_0000,
            load_address: 0x1_1000_0000,
            segments: vec![(0x1_0000_0000, 0x1_1000_0000, 0x1000), (0x1_0000_2000, 0x1_1000_2000, 0x1000)],
        };
        assert_eq!(mapping.runtime_address(0x1_0000_0020), Some(Address::new(0x1_1000_0020)));
        assert_eq!(mapping.file_address(Address::new(0x1_1000_2020)), Some(0x1_0000_2020));
        assert_eq!(mapping.runtime_address(0x1_0000_1000), None);
        assert_eq!(mapping.file_address(Address::new(0x1_1000_1000)), None);
        assert_eq!(mapping.runtime_address(0), None);
        assert_eq!(mapping.file_address(Address::new(0)), None);
        assert_eq!(mapping.file_address(Address::new(0x1_1000_3000)), None);
    }

    #[test]
    fn relocation_checks_overflow_and_supports_negative_slide()
    {
        let mapping = ImageMapping {
            file_base: 0x1_0000_0000,
            load_address: 0x1000,
            segments: vec![],
        };
        assert_eq!(mapping.relocate(0x1_0000_0040), Some(0x1040));
        assert_eq!(mapping.relocate(0), None);
        let mapping = ImageMapping {
            file_base: 0,
            load_address: u64::MAX - 10,
            segments: vec![],
        };
        assert_eq!(mapping.relocate(11), None);
    }
}

#[cfg(test)]
mod universal_tests
{
    use super::*;
    use crate::symbols::SymbolCache;
    use crate::symbols::lines::synthetic;

    const ARM64: u32 = 0x0100_000c;
    const X86_64: u32 = 0x0100_0007;
    /// arm64e with the pointer-authentication ABI capability bit, as in `/usr/lib/dyld`.
    const ARM64E: u32 = 0x8000_0002;

    /// A little-endian 64-bit Mach-O with an `LC_UUID`, a `__TEXT` segment at `text`
    /// covering the whole file, and three one-byte DWARF sections, so it parses with
    /// embedded debug info and no tool.
    fn slice(cpu_type: u32, cpu_subtype: u32, uuid: [u8; 16], text: u64) -> Vec<u8>
    {
        let sections = [
            ("__debug_info", &[0][..]),
            ("__debug_abbrev", &[0][..]),
            ("__debug_line", &[0][..]),
        ];
        slice_with_sections(cpu_type, cpu_subtype, uuid, text, &sections)
    }

    fn slice_with_sections(cpu_type: u32, cpu_subtype: u32, uuid: [u8; 16], text: u64, sections: &[(&str, &[u8])])
    -> Vec<u8>
    {
        let dwarf_size = 72 + 80 * sections.len();
        let commands_size = 24 + 72 + dwarf_size;
        let mut data_offset = 32 + commands_size;
        let len = data_offset + sections.iter().map(|(_, data)| data.len()).sum::<usize>();
        let mut bytes = vec![0; len];
        let word =
            |bytes: &mut [u8], offset: usize, value: u32| bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        let quad =
            |bytes: &mut [u8], offset: usize, value: u64| bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        word(&mut bytes, 0, 0xfeed_facf);
        word(&mut bytes, 4, cpu_type);
        word(&mut bytes, 8, cpu_subtype);
        word(&mut bytes, 12, 2);
        word(&mut bytes, 16, 3);
        word(&mut bytes, 20, u32::try_from(commands_size).unwrap());
        word(&mut bytes, 32, 0x1b);
        word(&mut bytes, 36, 24);
        bytes[40..56].copy_from_slice(&uuid);
        let segment = 56;
        word(&mut bytes, segment, 0x19);
        word(&mut bytes, segment + 4, 72);
        bytes[segment + 8..segment + 14].copy_from_slice(b"__TEXT");
        quad(&mut bytes, segment + 24, text);
        quad(&mut bytes, segment + 32, 0x4000);
        quad(&mut bytes, segment + 48, len as u64);
        let segment = 128;
        word(&mut bytes, segment, 0x19);
        word(&mut bytes, segment + 4, u32::try_from(dwarf_size).unwrap());
        bytes[segment + 8..segment + 15].copy_from_slice(b"__DWARF");
        word(&mut bytes, segment + 64, u32::try_from(sections.len()).unwrap());
        for (index, &(name, data)) in sections.iter().enumerate() {
            let section = segment + 72 + 80 * index;
            bytes[section..section + name.len()].copy_from_slice(name.as_bytes());
            bytes[section + 16..section + 23].copy_from_slice(b"__DWARF");
            quad(&mut bytes, section + 40, data.len() as u64);
            word(&mut bytes, section + 48, u32::try_from(data_offset).unwrap());
            bytes[data_offset..data_offset + data.len()].copy_from_slice(data);
            data_offset += data.len();
        }
        bytes
    }

    /// A 32-bit universal file holding `slices`, each aligned to 2^12.
    fn universal(slices: &[&[u8]], cpus: &[(u32, u32)]) -> Vec<u8>
    {
        let mut fat = vec![0; 0x1000];
        fat[..4].copy_from_slice(&0xcafe_babe_u32.to_be_bytes());
        fat[4..8].copy_from_slice(&u32::try_from(slices.len()).unwrap().to_be_bytes());
        for (index, (data, &(cpu_type, cpu_subtype))) in slices.iter().zip(cpus).enumerate() {
            let offset = fat.len();
            let header = 8 + 20 * index;
            for (field, value) in [
                cpu_type,
                cpu_subtype,
                u32::try_from(offset).unwrap(),
                u32::try_from(data.len()).unwrap(),
                12,
            ]
            .into_iter()
            .enumerate()
            {
                fat[header + 4 * field..header + 4 * field + 4].copy_from_slice(&value.to_be_bytes());
            }
            fat.extend_from_slice(data);
            fat.resize(fat.len().next_multiple_of(0x1000), 0);
        }
        fat
    }

    fn write(name: &str, bytes: &[u8]) -> PathBuf
    {
        let path = std::env::temp_dir().join(format!("ferros-universal-{name}-{}", std::process::id()));
        fs::write(&path, bytes).unwrap();
        path
    }

    fn parse(path: &Path, target: Option<&SliceIdentity>) -> Result<BinaryImage>
    {
        let options = DebugInfoOptions {
            dsymutil: None,
            ..DebugInfoOptions::default()
        };
        let descriptor = ImageDescriptor {
            path: path.to_path_buf(),
            load_address: 0x5_0000_0000,
        };
        BinaryImage::parse(descriptor, target, &options)
    }

    /// The link-time address of the parsed image's header: which slice was read.
    fn text(image: &BinaryImage) -> Option<u64>
    {
        image.mapping.file_address(Address::new(0x5_0000_0000))
    }

    #[test]
    fn public_slice_selection_reads_matching_symbols_and_mapping()
    {
        let with_function = |subtype, uuid, text, name| {
            let mut unit = synthetic::TestUnit::new("/src", "example.rs");
            let entry = text + 0x100;
            unit.rows = vec![(entry, 1)];
            unit.end = entry + 4;
            unit.functions = vec![(name, vec![(entry, unit.end)], None)];
            let dwarf = synthetic::write(&[unit]);
            let sections = [
                ("__debug_info", dwarf.sections[&SectionId::DebugInfo].as_slice()),
                ("__debug_abbrev", dwarf.sections[&SectionId::DebugAbbrev].as_slice()),
                ("__debug_line", dwarf.sections[&SectionId::DebugLine].as_slice()),
                ("__debug_ranges", dwarf.sections[&SectionId::DebugRanges].as_slice()),
            ];
            slice_with_sections(ARM64, subtype, uuid, text, &sections)
        };
        let arm64 = with_function(0, [1; 16], 0x1_0000_0000, "plain_arm64");
        let arm64e = with_function(ARM64E, [2; 16], 0x2_0000_0000, "arm64e_only");
        let path = write("public-slice", &universal(&[&arm64, &arm64e], &[(ARM64, 0), (ARM64, ARM64E)]));
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x5_0000_0000,
        };
        let mut cache = SymbolCache::new();
        let plain = cache.load_image_slice(descriptor.clone(), &SliceIdentity::new(ARM64, 0, None));
        let selector = SliceIdentity::new(ARM64, 2, None);
        let arm64e_image = cache.load_image_slice(descriptor.clone(), &selector);
        let mapping = ImageMapping::parse_slice(&descriptor, &selector);
        let unselected = cache.load_image(descriptor);
        fs::remove_file(&path).unwrap();

        let runtime = Address::new(0x5_0000_0100);
        let plain = plain.expect("plain arm64 image");
        let arm64e_image = arm64e_image.expect("arm64e image");
        assert_ne!(plain.id(), arm64e_image.id());
        assert_eq!(plain.function_containing(runtime).unwrap().unwrap().name, "plain_arm64");
        assert_eq!(
            arm64e_image.function_containing(runtime).unwrap().unwrap().name,
            "arm64e_only"
        );
        assert_eq!(arm64e_image.file_address(runtime), Some(0x2_0000_0100));
        assert_eq!(mapping.unwrap().file_address(runtime), Some(0x2_0000_0100));
        assert!(matches!(unselected, Err(DebuggerError::NoDebugInfo { .. })));
    }

    #[test]
    fn public_slice_selection_refuses_the_wrong_uuid()
    {
        let arm64e = slice(ARM64, ARM64E, [1; 16], 0x2_0000_0000);
        let path = write("public-uuid", &universal(&[&arm64e], &[(ARM64, ARM64E)]));
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x5_0000_0000,
        };
        let mut cache = SymbolCache::new();
        let correct = SliceIdentity::new(ARM64, 2, Some([1; 16]));
        let wrong = SliceIdentity::new(ARM64, 2, Some([2; 16]));
        assert!(cache.load_image_slice(descriptor.clone(), &correct).is_ok());
        let assert_refused = |cache: &mut SymbolCache| {
            for error in [
                cache.load_image_slice(descriptor.clone(), &wrong).err(),
                ImageMapping::parse_slice(&descriptor, &wrong).err(),
            ] {
                let error = error.expect("another build");
                assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
                assert!(error.to_string().contains("another build"), "{error}");
            }
        };
        assert_refused(&mut cache);
        fs::write(&path, arm64e).unwrap();
        assert_refused(&mut cache);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_slice_the_target_mapped_is_parsed_by_cpu_type_and_subtype()
    {
        let x86 = slice(X86_64, 3, [1; 16], 0x3_0000_0000);
        let arm64 = slice(ARM64, 0, [2; 16], 0x1_0000_0000);
        let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
        let path = write(
            "subtype",
            &universal(&[&x86, &arm64, &arm64e], &[(X86_64, 3), (ARM64, 0), (ARM64, ARM64E)]),
        );
        let mapped = SliceIdentity::from_header(&arm64e).unwrap();
        assert_eq!(
            mapped,
            SliceIdentity {
                cpu_type: ARM64,
                cpu_subtype: ARM64E,
                uuid: Some([3; 16]),
            }
        );
        let arm64e_image = parse(&path, Some(&mapped));
        // Capability bits are not part of the subtype: a header without them still names
        // the arm64e slice, never the plain arm64 one that shares its object::Architecture.
        let without_capabilities = SliceIdentity {
            cpu_subtype: 2,
            uuid: None,
            ..mapped
        };
        let arm64e_by_subtype = parse(&path, Some(&without_capabilities));
        let arm64_image = parse(&path, Some(&SliceIdentity::from_header(&arm64).unwrap()));
        let _ = fs::remove_file(&path);

        let arm64e_image = arm64e_image.expect("arm64e slice");
        assert_eq!(text(&arm64e_image), Some(0x2_0000_0000));
        assert_eq!(arm64e_image.architecture(), Architecture::Arm64);
        assert_eq!(
            text(&arm64e_by_subtype.expect("arm64e slice by subtype")),
            Some(0x2_0000_0000)
        );
        assert_eq!(text(&arm64_image.expect("arm64 slice")), Some(0x1_0000_0000));
    }

    #[test]
    fn the_uuid_chooses_between_slices_of_the_same_cpu()
    {
        let first = slice(ARM64, ARM64E, [4; 16], 0x1_0000_0000);
        let second = slice(ARM64, ARM64E, [5; 16], 0x2_0000_0000);
        let bytes = universal(&[&first, &second], &[(ARM64, ARM64E), (ARM64, ARM64E)]);
        let path = Path::new("/universal");
        let target = SliceIdentity::from_header(&second).unwrap();
        assert_eq!(image_slice(path, &bytes, Some(&target)).unwrap(), &second[..]);
        let unknown_uuid = SliceIdentity { uuid: None, ..target };
        assert_eq!(image_slice(path, &bytes, Some(&unknown_uuid)).unwrap(), &first[..]);
        // A single image is read as it is when its UUID is the mapped one or unknown.
        assert_eq!(image_slice(path, &second, Some(&target)).unwrap(), &second[..]);
        assert_eq!(image_slice(path, &first, Some(&unknown_uuid)).unwrap(), &first[..]);
        assert_eq!(image_slice(path, &first, None).unwrap(), &first[..]);
    }

    /// The symbol cache keys a parse on the mapped UUID too: two builds at the same path
    /// and load address get their own entries, and the second load never returns the
    /// first's parse (#170).
    #[test]
    fn the_symbol_cache_keys_a_parse_on_the_mapped_uuid()
    {
        let first = slice(ARM64, ARM64E, [8; 16], 0x1_0000_0000);
        let second = slice(ARM64, ARM64E, [9; 16], 0x2_0000_0000);
        let path = write(
            "cache-key",
            &universal(&[&first, &second], &[(ARM64, ARM64E), (ARM64, ARM64E)]),
        );
        let mut cache = crate::symbols::SymbolCache::with_debug_info_options(DebugInfoOptions {
            dsymutil: None,
            ..DebugInfoOptions::default()
        });
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x5_0000_0000,
        };
        let load = |cache: &mut crate::symbols::SymbolCache, bytes: &[u8]| {
            let target = SliceIdentity::from_header(bytes).unwrap();
            cache.load_mapped_image(descriptor.clone(), Some(&target))
        };
        let first_image = load(&mut cache, &first);
        let second_image = load(&mut cache, &second);
        let _ = fs::remove_file(&path);

        let (first_image, second_image) = (first_image.expect("first build"), second_image.expect("second build"));
        assert_ne!(first_image.id(), second_image.id());
        assert_eq!(text(&first_image), Some(0x1_0000_0000));
        assert_eq!(text(&second_image), Some(0x2_0000_0000), "the second build's own parse");
    }

    /// A single-architecture file rebuilt while the old build stays mapped is another
    /// build: a retryable failure, never a parse labelled with the mapped UUID (#170).
    #[test]
    fn a_replaced_single_image_is_another_build()
    {
        let mapped = slice(ARM64, ARM64E, [6; 16], 0x1_0000_0000);
        let rebuilt = slice(ARM64, ARM64E, [7; 16], 0x1_0000_0000);
        let target = SliceIdentity::from_header(&mapped).unwrap();
        let path = write("replaced", &rebuilt);
        let parsed = parse(&path, Some(&target));
        let reparsed = parse(&path, Some(&SliceIdentity { uuid: None, ..target }));
        let _ = fs::remove_file(&path);
        let error = parsed.err().expect("another build");
        assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
        assert!(error.to_string().contains("another build"), "{error}");
        assert!(reparsed.is_ok(), "an unknown mapped UUID reads the file as it is");
    }

    #[test]
    fn a_universal_file_without_the_target_slice_has_no_debug_info()
    {
        let x86 = slice(X86_64, 3, [1; 16], 0x3_0000_0000);
        let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
        let path = write("missing", &universal(&[&x86, &arm64e], &[(X86_64, 3), (ARM64, ARM64E)]));
        let other_build = SliceIdentity {
            uuid: Some([9; 16]),
            ..SliceIdentity::from_header(&arm64e).unwrap()
        };
        let plain_arm64 = SliceIdentity {
            cpu_type: ARM64,
            cpu_subtype: 0,
            uuid: None,
        };
        let replaced = parse(&path, Some(&other_build)).err();
        let results = [
            parse(&path, Some(&plain_arm64)).err(),
            parse(&path, None).err(),
            crate::symbols::SymbolCache::with_debug_info_options(DebugInfoOptions {
                dsymutil: None,
                ..DebugInfoOptions::default()
            })
            .load_image(ImageDescriptor {
                path: path.clone(),
                load_address: 0x5_0000_0000,
            })
            .err(),
        ];
        let mapping = ImageMapping::parse(&ImageDescriptor {
            path: path.clone(),
            load_address: 0x5_0000_0000,
        });
        let _ = fs::remove_file(&path);
        for error in results {
            let error = error.expect("no slice to read");
            assert!(
                matches!(&error, DebuggerError::NoDebugInfo { path: reported, .. } if *reported == path),
                "{error}"
            );
            assert!(error.to_string().contains("universal"), "{error}");
        }
        // A slice of the mapped CPU from another build means the file was replaced: a
        // load failure, retried once the file changes, not debug info that is missing.
        let error = replaced.expect("another build");
        assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
        assert!(error.to_string().contains("another build"), "{error}");
        let error = mapping.expect_err("no slice to map");
        assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
        assert!(error.to_string().contains("universal"), "{error}");
    }

    /// With the mapped UUID known, a candidate slice that is not a valid Mach-O is
    /// reported as damaged, not as a build mismatch or missing debug info.
    #[test]
    fn a_damaged_candidate_slice_is_reported_as_damaged()
    {
        let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
        let target = SliceIdentity::from_header(&arm64e).unwrap();
        let damaged = vec![0xab; arm64e.len()];
        let bytes = universal(&[&damaged], &[(ARM64, ARM64E)]);
        let error = image_slice(Path::new("/universal"), &bytes, Some(&target)).unwrap_err();
        assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
        assert!(!error.to_string().contains("another build"), "{error}");
    }

    #[test]
    fn a_malformed_universal_file_is_a_parse_error()
    {
        let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
        let mut bytes = universal(&[&arm64e], &[(ARM64, ARM64E)]);
        bytes.truncate(0x1000 + arm64e.len() / 2);
        let target = SliceIdentity::from_header(&arm64e).unwrap();
        let error = image_slice(Path::new("/universal"), &bytes, Some(&target)).unwrap_err();
        assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
    }
}
