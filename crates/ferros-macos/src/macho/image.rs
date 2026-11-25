//! Mach-O containers for the engine: the segment layout of a mapped header, the slice
//! of a universal file the target mapped, and the debug and unwind sections of an image's
//! file.
//!
//! [`ImageMapping`] is built from `LC_SEGMENT_64` commands, without `__PAGEZERO` (which
//! reserves address space and maps nothing). A [`SliceIdentity`] is the mapped header's
//! CPU type, CPU subtype and `LC_UUID`; it is the [`ImageSlice`] selector, and selects the
//! slice of a universal (fat) file to read (#169, #175). The file's DWARF comes from the
//! image or a companion ([`super::debuginfo`]); `__eh_frame`, `__eh_frame_hdr` and
//! `__unwind_info` come from the image itself.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use ferros_core::error::{DebuggerError, Result};
use ferros_core::objfile::{
    DWARF_SECTIONS, DebugInfoRetry, DebugSections, ImageContents, ImageSlice, RunTimeEndian, SectionData, SectionId,
};
use ferros_core::symbols::{ImageMapping, ImageSegment};
use ferros_core::types::{Architecture, BuildId, BuildIdentity};
use object::macho::MachHeader64;
use object::read::macho::{MachHeader as _, Segment as _};
use object::{Endianness, Object, ObjectSection, ObjectSegment};

use super::compact_unwind::CompactUnwind;
use super::debuginfo::{self, DebugInfoOptions};

/// One segment load command, from a file or from mapped load commands.
struct SegmentLayout
{
    is_page_zero: bool,
    segment: ImageSegment,
}

fn mapping_of(segments: &[SegmentLayout], load_address: u64) -> Result<ImageMapping>
{
    let segments: Vec<_> = segments
        .iter()
        .filter(|layout| !layout.is_page_zero)
        .map(|layout| layout.segment)
        .collect();
    ImageMapping::from_segments(&segments, load_address)
}

/// The segment layout from a 64-bit Mach-O header and its load commands, as mapped at
/// `load_address` in the target. `header` must start at the header and hold its
/// `sizeofcmds` bytes of load commands.
///
/// # Errors
/// Returns an error for a truncated or malformed header or overflowing mappings.
pub(crate) fn mapping_from_header(header: &[u8], load_address: u64) -> Result<ImageMapping>
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
                segment: ImageSegment {
                    address: segment.vmaddr(endian),
                    size: segment.vmsize(endian),
                    file_offset: segment.fileoff(endian),
                    file_size: segment.filesize(endian),
                },
            });
        }
    }
    mapping_of(&segments, load_address)
}

fn mapping_from_file(file: &object::File<'_>, load_address: u64) -> Result<ImageMapping>
{
    let segments: Vec<_> = file
        .segments()
        .map(|segment| SegmentLayout {
            is_page_zero: segment.name().ok().flatten() == Some("__PAGEZERO"),
            segment: ImageSegment {
                address: segment.address(),
                size: segment.size(),
                file_offset: segment.file_range().0,
                file_size: segment.file_range().1,
            },
        })
        .collect();
    mapping_of(&segments, load_address)
}

/// Read an executable's segment layout from its file without discovering debug
/// information.
///
/// # Errors
/// Returns an error for unreadable or malformed files, universal files (which slice the
/// target mapped is unknown) or overflowing mappings.
pub fn file_mapping(path: &Path, load_address: u64) -> Result<ImageMapping>
{
    parse_file_mapping(path, load_address, None)
}

/// Read the segment layout of the selected slice of a universal file without
/// discovering debug information (#175).
///
/// # Errors
/// Returns an error for unreadable or malformed files, a missing slice, a UUID mismatch,
/// or overflowing mappings.
pub fn slice_mapping(path: &Path, load_address: u64, selector: &SliceIdentity) -> Result<ImageMapping>
{
    parse_file_mapping(path, load_address, Some(selector))
}

fn parse_file_mapping(path: &Path, load_address: u64, selector: Option<&SliceIdentity>) -> Result<ImageMapping>
{
    let bytes = fs::read(path)?;
    let data = match selector {
        Some(selector) => image_slice(path, &bytes, Some(selector))?,
        None if debuginfo::universal_slices(&bytes).is_some() => {
            return Err(DebuggerError::InvalidArgument(format!(
                "{} is a universal file: which slice the target mapped is unknown; read the mapped header instead",
                path.display()
            )));
        }
        None => bytes.as_slice(),
    };
    let file =
        object::File::parse(data).map_err(|error| DebuggerError::InvalidArgument(format!("invalid image: {error}")))?;
    mapping_from_file(&file, load_address)
}

/// Selects a slice of a universal (fat) Mach-O file by its CPU type, CPU subtype, and
/// optional `LC_UUID` (#175).
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

/// Bytes of an [`ImageSlice`] selector: `cputype`, masked `cpusubtype` (both
/// little-endian).
const SELECTOR_SIZE: usize = 8;

impl SliceIdentity
{
    /// Create a selector from Mach-O `cputype`, `cpusubtype`, and an optional `LC_UUID`
    /// (#175). The UUID distinguishes builds of the same CPU type and subtype.
    ///
    /// ```rust
    /// use ferros_macos::macho::SliceIdentity;
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

    /// Read the identity from a 64-bit Mach-O header and its load commands.
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
            cpu_type: mach.cputype(endian).0,
            cpu_subtype: mach.cpusubtype(endian).0,
            uuid,
        })
    }

    /// The engine's view of this slice. The selector holds the CPU type and the subtype
    /// without capability bits, the build id the UUID (unknown without one: a header read
    /// that found no `LC_UUID` is not evidence that the format records none).
    #[must_use]
    pub fn to_image_slice(&self) -> ImageSlice
    {
        let mut selector = [0; SELECTOR_SIZE];
        selector[..4].copy_from_slice(&self.cpu_type.to_le_bytes());
        selector[4..].copy_from_slice(&base_subtype(self.cpu_subtype).to_le_bytes());
        let build_id = self
            .uuid
            .map_or(BuildIdentity::Unknown, |uuid| BuildIdentity::Known(BuildId::from(uuid)));
        ImageSlice::new(selector.to_vec(), build_id)
    }

    /// The selector a [`to_image_slice`](Self::to_image_slice) result stands for, with a
    /// UUID when its build id is a 16-byte one. `None` for a slice this format did not
    /// make.
    pub(crate) fn from_image_slice(slice: &ImageSlice) -> Option<Self>
    {
        let selector: [u8; SELECTOR_SIZE] = slice.selector().try_into().ok()?;
        let word = |at: usize| selector[at..at + 4].try_into().ok().map(u32::from_le_bytes);
        Some(Self {
            cpu_type: word(0)?,
            cpu_subtype: word(4)?,
            uuid: slice.build_id().known().and_then(|id| id.as_bytes().try_into().ok()),
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
    let no_slice = |detail: String| debuginfo::no_debug_info(path, detail);
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

fn section_bytes(section: &object::Section<'_, '_>, name: &str) -> Result<Arc<[u8]>>
{
    let data = section
        .uncompressed_data()
        .map_err(|err| DebuggerError::InvalidArgument(format!("failed to read {name}: {err}")))?;
    Ok(match data {
        Cow::Borrowed(bytes) => Arc::<[u8]>::from(bytes.to_vec()),
        Cow::Owned(vec) => vec.into(),
    })
}

/// The section `name`, or its Mach-O spelling (`__name` for `.name`), with its address.
fn section_data(file: &object::File<'_>, names: &[&str]) -> Result<Option<SectionData>>
{
    for name in names {
        if let Some(section) = file.section_by_name(name) {
            return Ok(Some(SectionData {
                address: section.address(),
                data: section_bytes(&section, name)?,
            }));
        }
    }
    Ok(None)
}

/// The Mach-O spellings of a DWARF section, the ELF-style name first.
fn dwarf_names(id: SectionId) -> [String; 2]
{
    let name = id.name();
    [name.to_owned(), format!("__{}", name.trim_start_matches('.'))]
}

/// Read the image at `path`, mapped at `load_address`, for the engine. For a universal
/// file the slice `target` names is read ([`image_slice`]); without one, a universal file
/// has no usable debug info.
///
/// The image's own layout, `__eh_frame`, `__eh_frame_hdr` and `__unwind_info` are read
/// before its DWARF is discovered, so an executable without a
/// companion debug file still unwinds: the discovery failure is
/// [`ImageContents::debug`], not an error of this call.
pub(crate) fn open(
    path: &Path,
    load_address: u64,
    target: Option<&SliceIdentity>,
    options: &DebugInfoOptions,
) -> Result<(ImageContents, Option<DebugInfoRetry>)>
{
    let bytes = fs::read(path)?;
    let data = image_slice(path, &bytes, target)?;
    let file = object::File::parse(data)
        .map_err(|err| DebuggerError::InvalidArgument(format!("failed to parse {}: {err}", path.display())))?;

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

    let mapping = mapping_from_file(&file, load_address)?;
    let unwind_hints = section_data(&file, &["__unwind_info"])?
        .map(|section| Box::new(CompactUnwind::new(section.data, load_address)) as Box<_>);
    let eh_frame = section_data(&file, &[".eh_frame", "__eh_frame"])?;
    let eh_frame_hdr = section_data(&file, &[".eh_frame_hdr", "__eh_frame_hdr"])?;
    let (debug, retry_debug) = image_debug_info(path, &file, options);
    Ok((
        ImageContents {
            architecture,
            endian,
            mapping,
            eh_frame,
            eh_frame_hdr,
            unwind_hints,
            debug,
        },
        retry_debug,
    ))
}

/// Read embedded DWARF or discover a companion. A failed companion lookup can
/// be repeated using the validated slice's UUID, architecture and debug-map status.
fn image_debug_info(
    path: &Path,
    file: &object::File<'_>,
    options: &DebugInfoOptions,
) -> (Result<DebugSections>, Option<DebugInfoRetry>)
{
    let search = match debuginfo::CompanionSearch::for_executable(path, file) {
        Ok(None) => return (debug_sections(file), None),
        Ok(Some(search)) => search,
        Err(error) => return (Err(error), None),
    };
    let debug = companion_sections(path, search, options, || debuginfo::has_debug_map(file));
    let retry: Option<DebugInfoRetry> = match &debug {
        Ok(_) | Err(DebuggerError::NoDebugInfo { .. }) => None,
        Err(_) => {
            let path = path.to_path_buf();
            let options = options.clone();
            let has_map = debuginfo::has_debug_map(file);
            Some(Arc::new(move || companion_sections(&path, search, &options, || has_map)))
        }
    };
    (debug, retry)
}

fn companion_sections(
    path: &Path,
    search: debuginfo::CompanionSearch,
    options: &DebugInfoOptions,
    has_map: impl FnOnce() -> bool,
) -> Result<DebugSections>
{
    let bytes = debuginfo::discover_companion(path, search, options, has_map)?;
    let file = object::File::parse(&*bytes)
        .map_err(|error| DebuggerError::InvalidArgument(format!("failed to parse DWARF: {error}")))?;
    debug_sections(&file)
}

/// The source DWARF and `__debug_frame` of a file containing debug information.
fn debug_sections(debug_file: &object::File<'_>) -> Result<DebugSections>
{
    let mut dwarf = HashMap::new();
    for &id in DWARF_SECTIONS {
        let names = dwarf_names(id);
        if let Some(section) = names.iter().find_map(|name| debug_file.section_by_name(name)) {
            dwarf.insert(id, section_bytes(&section, id.name())?);
        }
    }
    Ok(DebugSections {
        dwarf,
        debug_frame: section_data(debug_file, &[".debug_frame", "__debug_frame"])?,
    })
}

#[cfg(test)]
mod tests;
