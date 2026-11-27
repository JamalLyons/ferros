//! Binary image parsing and DWARF section loading.
//!
//! This module turns what an [`ObjectFormat`](crate::objfile::ObjectFormat) read from an
//! image's file ([`ImageContents`]) into a [`BinaryImage`]: the image's DWARF and call
//! frame information, indexed for lookups. It handles:
//!
//! - DWARF section access (`.debug_info`, `.debug_line`, etc.) by [`SectionId`]
//! - Address relocation (mapping runtime addresses to file addresses)
//! - Symbolication (mapping addresses to function names and source locations)
//! - Type information extraction
//!
//! The container itself (headers, segments, where the sections are, companion debug
//! files) is the format's business; this module never parses it.
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
//!
//! ## Map of this file
//!
//! - `ImageDescriptor`: what a caller supplies to load an image.
//! - `ImageMapping`, `ImageSegment`: file-to-runtime address translation (`relocate`,
//!   `file_address`, `runtime_address`).
//! - `ImageId`: stable identity of a loaded image.
//! - `BinaryImage`: construction (`from_contents`), layout queries, call frame sections
//!   (`eh_frame_section`, `debug_frame_section`), `unwind_hints`, `symbolicate`, and
//!   `types` (the image's [`TypeTable`]). Line and function lookups on `BinaryImage` live
//!   in `lines.rs` and `functions.rs`.
//! - Mapping tests in `mod mapping_tests`.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use addr2line::Context;
use gimli::{Dwarf, EndianArcSlice, RunTimeEndian, SectionId};
use once_cell::sync::OnceCell;

use super::cache::{SymbolFrame, Symbolication};
use super::demangle::make_symbol_name;
use super::functions::FunctionIndex;
use super::lines::{LineIndex, SourceLookupError};
use super::{OwnedDwarf, OwnedReader};
use crate::error::{DebuggerError, Result};
use crate::objfile::{DebugInfoRetry, DebugSections, ImageContents, ImageSlice, LoadInputs, SectionData, UnwindHints};
use crate::types::{Address, Architecture, FunctionParameter, SourceLocation, SymbolSource};
use crate::values::types::TypeTable;

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

/// Checked address translation for an image's mapped segments.
///
/// This contains no DWARF and can therefore be used even for stripped images.
/// `load_address` is the runtime address of the image's first byte (its header); the
/// corresponding file virtual address comes from the segment covering file offset zero.
///
/// An [`ObjectFormat`](crate::objfile::ObjectFormat) builds one with
/// [`from_segments`](Self::from_segments), from the header the loader mapped (which stays
/// valid while the file is replaced or removed) or from the file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMapping
{
    file_base: u64,
    load_address: u64,
    segments: Vec<(u64, u64, u64)>,
}

/// One segment of an image, as its format describes it, for
/// [`ImageMapping::from_segments`]. Addresses are file (link-time) virtual addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSegment
{
    /// The segment's link-time virtual address.
    pub address: u64,
    /// The segment's size in memory.
    pub size: u64,
    /// Offset of the segment's bytes in the file.
    pub file_offset: u64,
    /// Bytes of the segment backed by the file.
    pub file_size: u64,
}

impl ImageMapping
{
    /// The layout of an image whose header is mapped at `load_address`, from its
    /// `segments`. The segment at file offset zero with file bytes gives the slide;
    /// segments of size zero are left out. A format leaves out segments that reserve
    /// address space without mapping the image (a guard region at address zero).
    ///
    /// # Errors
    /// Returns an error when no segment covers the header or a segment overflows the
    /// address space once relocated.
    pub fn from_segments(segments: &[ImageSegment], load_address: u64) -> Result<Self>
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
            if segment.size == 0 {
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

    /// The runtime address of the file virtual address `address`, by the image's slide,
    /// whether or not a segment covers it; `None` on overflow.
    #[must_use]
    pub fn relocate(&self, address: u64) -> Option<u64>
    {
        u64::try_from(i128::from(address) + i128::from(self.load_address) - i128::from(self.file_base)).ok()
    }

    /// The runtime address of the image's header.
    #[must_use]
    pub const fn load_address(&self) -> u64
    {
        self.load_address
    }

    pub(crate) fn runtime_range(&self) -> (u64, u64)
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

/// Unique identifier for a binary image.
///
/// The symbol cache keys on path and load address. When the session knows which slice
/// and build of the file is mapped ([`ImageSlice`]), that also contributes to the key
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

    pub(crate) fn with_slice(path: &Path, load_address: u64, slice: Option<&ImageSlice>) -> Self
    {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        load_address.hash(&mut hasher);
        if let Some(slice) = slice {
            slice.hash(&mut hasher);
        }
        ImageId(hasher.finish())
    }

    /// Get the raw `u64` value of this image ID.
    ///
    /// This is useful for serialization or debugging. The value hashes the path,
    /// load address, and any slice.
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
    executable: Arc<ExecutableImage>,
    /// The image's source debug information, or why there is none. The executable half
    /// above is usable for unwinding either way.
    debug: Result<DebugSections>,
    retry_debug: Option<DebugInfoRetry>,
    dwarf_cache: OnceCell<OwnedDwarf>,
    /// `addr2line::Context` is not `Sync`; the mutex makes `BinaryImage` `Send + Sync`
    /// so a `SymbolCache` (and the `Session` holding it) can move between threads.
    context_cache: OnceCell<Mutex<Context<OwnedReader>>>,
    /// Built on the first type lookup (see `values::types`); a failure to load the DWARF
    /// sections is cached too.
    types: OnceCell<std::result::Result<TypeTable, SourceLookupError>>,
    /// Built on the first source lookup (see `symbols::lines`); a failure to load the
    /// DWARF sections is cached too.
    line_index: OnceCell<std::result::Result<LineIndex, SourceLookupError>>,
    /// Built on the first function lookup (see `symbols::functions`); a failure to load the
    /// DWARF sections is cached too.
    function_index: OnceCell<std::result::Result<FunctionIndex, SourceLookupError>>,
}

struct ExecutableImage
{
    id: ImageId,
    path: PathBuf,
    architecture: Architecture,
    endian: RunTimeEndian,
    mapping: ImageMapping,
    eh_frame: Option<SectionData>,
    eh_frame_hdr: Option<SectionData>,
    /// The format's own per-function unwind table, consulted before the CFI.
    unwind_hints: Option<Box<dyn UnwindHints>>,
}

impl BinaryImage
{
    /// The image `desc` names, from what its format read of its file. `slice` is the
    /// mapped slice the contents were read for, part of the image's id.
    pub(crate) fn from_contents(
        desc: ImageDescriptor,
        slice: Option<&ImageSlice>,
        contents: ImageContents,
        retry_debug: Option<DebugInfoRetry>,
    ) -> Self
    {
        let executable = Arc::new(ExecutableImage {
            id: ImageId::with_slice(&desc.path, desc.load_address, slice),
            path: desc.path,
            architecture: contents.architecture,
            endian: contents.endian,
            mapping: contents.mapping,
            eh_frame: contents.eh_frame,
            eh_frame_hdr: contents.eh_frame_hdr,
            unwind_hints: contents.unwind_hints,
        });
        Self::new(executable, contents.debug, retry_debug)
    }

    fn new(executable: Arc<ExecutableImage>, debug: Result<DebugSections>, retry_debug: Option<DebugInfoRetry>) -> Self
    {
        Self {
            executable,
            debug,
            retry_debug,
            dwarf_cache: OnceCell::new(),
            context_cache: OnceCell::new(),
            types: OnceCell::new(),
            line_index: OnceCell::new(),
            function_index: OnceCell::new(),
        }
    }

    /// Refresh the source debug information and its indexes over the same executable.
    pub(super) fn retry_debug_info(&self) -> Option<Self>
    {
        let (debug, retry) = self.retry_debug.as_ref()?.retry();
        Some(Self::new(self.executable.clone(), debug, Some(retry)))
    }

    pub(super) fn debug_load_inputs(&self) -> LoadInputs
    {
        self.retry_debug
            .as_ref()
            .map_or_else(LoadInputs::default, |retry| retry.inputs.clone())
    }

    pub(crate) fn runtime_range(&self) -> (u64, u64)
    {
        self.executable.mapping.runtime_range()
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
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// assert_eq!(image.pointer_size(), 8); // 64-bit
    /// ```
    pub fn pointer_size(&self) -> u8
    {
        self.executable.architecture.pointer_size_bytes()
    }

    pub(crate) fn endian(&self) -> RunTimeEndian
    {
        self.executable.endian
    }

    pub(crate) fn relocated_address(&self, vmaddr: u64) -> u64
    {
        self.executable.mapping.relocate(vmaddr).unwrap_or(0)
    }

    pub(crate) fn eh_frame_section(&self) -> Option<(u64, &[u8])>
    {
        self.executable
            .eh_frame
            .as_ref()
            .map(|blob| (blob.address, blob.data.as_ref()))
    }

    pub(crate) fn eh_frame_hdr_section(&self) -> Option<(u64, &[u8])>
    {
        self.executable
            .eh_frame_hdr
            .as_ref()
            .map(|blob| (blob.address, blob.data.as_ref()))
    }

    pub(crate) fn debug_frame_section(&self) -> Option<(u64, &[u8])>
    {
        let debug_frame = self.debug.as_ref().ok()?.debug_frame.as_ref()?;
        Some((debug_frame.address, debug_frame.data.as_ref()))
    }

    /// Whether the image has source debug information. Without it the image still
    /// unwinds from its own unwind tables, but names no function and maps no line.
    #[must_use]
    pub fn has_debug_info(&self) -> bool
    {
        self.debug.is_ok()
    }

    /// Why the image has no source debug information, or `None` when it has some.
    pub(crate) fn debug_error(&self) -> Option<&DebuggerError>
    {
        self.debug.as_ref().err()
    }

    fn debug_sections(&self) -> Result<&DebugSections>
    {
        self.debug.as_ref().map_err(DebuggerError::duplicate)
    }

    pub(crate) fn unwind_hints(&self) -> Option<&dyn UnwindHints>
    {
        self.executable.unwind_hints.as_deref()
    }

    /// Get the unique identifier for this image.
    ///
    /// The ID is computed from the image's path, load address, and any slice.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
    ///
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let id = image.id();
    /// ```
    pub fn id(&self) -> ImageId
    {
        self.executable.id
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
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
    /// let descriptor = ImageDescriptor {
    ///     path: std::path::PathBuf::from("/usr/bin/prog"),
    ///     load_address: 0x100000000,
    /// };
    /// let image = cache.load_image(descriptor).unwrap();
    /// let path = image.path();
    /// ```
    pub fn path(&self) -> &Path
    {
        &self.executable.path
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
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
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
        self.executable.architecture
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
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
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
        self.executable.mapping.contains(address)
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
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
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
        self.executable.mapping.file_address(address)
    }

    /// Convert a file virtual address within a mapped segment to a runtime address.
    pub fn runtime_address(&self, address: u64) -> Option<Address>
    {
        self.executable.mapping.runtime_address(address)
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

    /// The image's DWARF, loaded on first use.
    ///
    /// # Errors
    /// Why the image has no source debug information, or why its DWARF does not load.
    pub(super) fn dwarf(&self) -> Result<&OwnedDwarf>
    {
        let sections = self.debug_sections()?;
        self.dwarf_cache.get_or_try_init(|| {
            Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(sections, section)))
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to load DWARF: {err}")))
        })
    }

    fn section_reader(&self, sections: &DebugSections, id: SectionId) -> OwnedReader
    {
        let data = sections
            .dwarf
            .get(&id)
            .cloned()
            .unwrap_or_else(|| Arc::<[u8]>::from(Vec::new()));
        EndianArcSlice::new(data, self.executable.endian)
    }

    fn symbol_context(&self) -> Result<&Mutex<Context<OwnedReader>>>
    {
        let sections = self.debug_sections()?;
        self.context_cache.get_or_try_init(|| {
            let dwarf = Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(sections, section)))
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
    /// # let format = ferros_core::objfile::mock::MockObjectFormat::shared();
    /// let mut cache = SymbolCache::new(format);
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
                debug!(
                    "Address 0x{:x} not in image {}",
                    address.value(),
                    self.executable.path.display()
                );
                return None;
            }
        };

        let ctx = match self.symbol_context() {
            Ok(ctx) => ctx.lock().unwrap_or_else(PoisonError::into_inner),
            Err(e) => {
                use tracing::debug;
                debug!("Failed to get symbol context for {}: {}", self.executable.path.display(), e);
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
                    self.executable.path.display(),
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
            debug!(
                "No frames found for address 0x{:x} in {}",
                file_addr,
                self.executable.path.display()
            );
            return None;
        }

        Some(Symbolication {
            image_id: self.executable.id,
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

    /// The image's types, resolved from its DWARF on demand (see
    /// [`values::types`](crate::values::types)). The name index is built on the first call
    /// and cached, and so is a failure to load the DWARF sections.
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::DebugInfo`] when the image has no debug information or its
    /// DWARF sections cannot be loaded.
    pub fn types(&self) -> std::result::Result<&TypeTable, SourceLookupError>
    {
        self.types
            .get_or_init(|| {
                let sections = self
                    .debug_sections()
                    .map_err(|error| SourceLookupError::DebugInfo(error.to_string()))?;
                let dwarf = Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(sections, section)))
                    .map_err(|error| SourceLookupError::DebugInfo(format!("failed to load DWARF: {error}")))?;
                let table = TypeTable::build(dwarf, self.executable.endian);
                super::lines::warn_skipped(self, "type", table.skipped());
                Ok(table)
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

#[cfg(test)]
mod mapping_tests
{
    use super::*;

    fn segment(address: u64, size: u64, file_offset: u64, file_size: u64) -> ImageSegment
    {
        ImageSegment {
            address,
            size,
            file_offset,
            file_size,
        }
    }

    #[test]
    fn mapping_uses_the_header_segment_skips_empty_ones_and_rejects_overflow()
    {
        let segments = [
            segment(0, 0, 0, 0),
            segment(0x1_0000_0000, 0x1000, 0, 248),
            segment(0x1_0000_1800, 0, 0x800, 0),
            segment(0x1_0000_2000, 0x1000, 0x1000, 0),
        ];
        let mapping = ImageMapping::from_segments(&segments, 0x1000).unwrap();
        assert_eq!(mapping.file_address(Address::new(0x1020)), Some(0x1_0000_0020));
        assert_eq!(mapping.runtime_address(0x1_0000_2020), Some(Address::new(0x3020)));
        assert_eq!(mapping.runtime_address(0), None);
        assert_eq!(mapping.runtime_address(0x1_0000_1000), None);
        assert_eq!(mapping.load_address(), 0x1000);
        assert!(ImageMapping::from_segments(&segments, u64::MAX - 0x100).is_err());
        assert!(
            ImageMapping::from_segments(&segments[2..], 0x1000).is_err(),
            "no segment covers the header"
        );
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
