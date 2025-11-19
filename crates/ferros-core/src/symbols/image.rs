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
use std::sync::{Arc, RwLock};

use addr2line::Context;
use gimli::{Dwarf, EndianArcSlice, RunTimeEndian, SectionId};
use object::{Object, ObjectSection, ObjectSegment};
use once_cell::sync::OnceCell;

use super::cache::{SymbolFrame, Symbolication};
use super::demangle::make_symbol_name;
use super::extractor::{TypeExtractor, TypeSummary};
use super::{OwnedDwarf, OwnedReader};
use crate::error::{DebuggerError, Result};
use crate::types::{Address, Architecture, SourceLocation};

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
    /// This is the base address where the binary's `__TEXT` segment (or equivalent)
    /// is loaded at runtime. Used to compute address relocation.
    pub load_address: u64,
}

/// Unique identifier for a binary image.
///
/// This ID is computed from the image's path and load address, ensuring that
/// the same binary loaded at different addresses gets different IDs. It's used
/// as a key in the symbol cache to distinguish between multiple instances of
/// the same binary.
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
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        load_address.hash(&mut hasher);
        ImageId(hasher.finish())
    }

    /// Get the raw `u64` value of this image ID.
    ///
    /// This is useful for serialization or debugging. The value is a hash
    /// of the path and load address.
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
    slide: i64,
    runtime_range: (u64, u64),
    debug_sections: HashMap<&'static str, Arc<[u8]>>,
    eh_frame: Option<SectionBlob>,
    eh_frame_hdr: Option<SectionBlob>,
    debug_frame: Option<SectionBlob>,
    dwarf_cache: OnceCell<OwnedDwarf>,
    context_cache: OnceCell<Context<OwnedReader>>,
    type_cache: RwLock<HashMap<String, Arc<TypeSummary>>>,
}

impl BinaryImage
{
    pub(crate) fn parse(desc: ImageDescriptor) -> Result<Self>
    {
        let bytes = fs::read(&desc.path)?;
        let data = Arc::<[u8]>::from(bytes);
        let file = object::File::parse(&*data)
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

        let text_segment = file
            .segments()
            .find(|segment| {
                if let Ok(Some(name)) = segment.name() {
                    name == "__TEXT" || name == ".text"
                } else {
                    false
                }
            })
            .ok_or_else(|| DebuggerError::InvalidArgument(format!("{} missing __TEXT segment", desc.path.display())))?;

        let text_vmaddr = text_segment.address();
        let mut max_addr = text_vmaddr;
        for segment in file.segments() {
            let start = segment.address();
            let end = start.saturating_add(segment.size());
            max_addr = max_addr.max(end);
        }

        let size = max_addr.saturating_sub(text_vmaddr);
        let runtime_start = desc.load_address;
        let runtime_end = runtime_start.saturating_add(size);
        let slide = desc.load_address as i64 - text_vmaddr as i64;

        let mut sections = HashMap::new();
        for (canonical, aliases) in DWARF_SECTIONS {
            let data = load_section_bytes(&file, aliases)?;
            sections.insert(*canonical, data);
        }

        let eh_frame = load_section_blob(&file, &[".eh_frame", "__eh_frame"])?;
        let eh_frame_hdr = load_section_blob(&file, &[".eh_frame_hdr", "__eh_frame_hdr"])?;
        let debug_frame = load_section_blob(&file, &[".debug_frame", "__debug_frame"])?;

        Ok(Self {
            id: ImageId::from_parts(&desc.path, desc.load_address),
            path: desc.path,
            architecture,
            endian,
            slide,
            runtime_range: (runtime_start, runtime_end),
            debug_sections: sections,
            eh_frame,
            eh_frame_hdr,
            debug_frame,
            dwarf_cache: OnceCell::new(),
            context_cache: OnceCell::new(),
            type_cache: RwLock::new(HashMap::new()),
        })
    }

    pub(crate) fn runtime_range(&self) -> (u64, u64)
    {
        self.runtime_range
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
        if self.slide >= 0 {
            vmaddr.saturating_add(self.slide as u64)
        } else {
            vmaddr.saturating_sub((-self.slide) as u64)
        }
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

    /// Get the unique identifier for this image.
    ///
    /// The ID is computed from the image's path and load address.
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
        let addr = address.value();
        addr >= self.runtime_range.0 && addr < self.runtime_range.1
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
        let value = address.value();
        if !self.contains(address) {
            return None;
        }

        if self.slide >= 0 {
            value.checked_sub(self.slide as u64)
        } else {
            value.checked_add((-self.slide) as u64)
        }
    }

    fn dwarf(&self) -> Result<&OwnedDwarf>
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

    fn symbol_context(&self) -> Result<&Context<OwnedReader>>
    {
        self.context_cache.get_or_try_init(|| {
            let dwarf = Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(section)))
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to load DWARF for addr2line: {err}")))?;
            Context::from_dwarf(dwarf)
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
        let file_addr = self.file_address(address)?;
        let ctx = self.symbol_context().ok()?;
        let mut frames = Vec::new();

        let lookup = ctx.find_frames(file_addr);
        let mut frame_iter = match lookup.skip_all_loads() {
            Ok(iter) => iter,
            Err(_) => return None,
        };

        while let Ok(Some(frame)) = frame_iter.next() {
            let symbol_name = frame
                .function
                .as_ref()
                .and_then(|func| func.raw_name().ok())
                .map(|raw| make_symbol_name(raw.to_string()));
            let location = frame.location.and_then(|loc| {
                loc.file.map(|file| SourceLocation {
                    file: file.to_string(),
                    line: loc.line,
                    column: loc.column,
                })
            });

            if let Some(symbol) = symbol_name {
                frames.push(SymbolFrame { symbol, location });
            }
        }

        if frames.is_empty() {
            return None;
        }

        Some(Symbolication {
            image_id: self.id,
            frames,
        })
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
