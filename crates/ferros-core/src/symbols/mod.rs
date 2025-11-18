use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use addr2line::Context;
use gimli::{
    self, constants, Attribute, AttributeValue, DebugTypeSignature, DebuggingInformationEntry, Dwarf, EndianArcSlice,
    Reader, RunTimeEndian, SectionId, Unit, UnitOffset, UnitSectionOffset, UnitType,
};
use object::{Object, ObjectSection, ObjectSegment};
use once_cell::sync::OnceCell;
use rustc_demangle::try_demangle;

use crate::error::{DebuggerError, Result};
use crate::types::{Address, Architecture, SourceLocation, SymbolLanguage, SymbolName};
type OwnedReader = EndianArcSlice<RunTimeEndian>;
type OwnedDwarf = Dwarf<OwnedReader>;

/// Describes a binary image mapped in the debuggee.
#[derive(Debug, Clone)]
pub struct ImageDescriptor
{
    pub path: PathBuf,
    pub load_address: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId(u64);

impl ImageId
{
    fn from_parts(path: &Path, load_address: u64) -> Self
    {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        load_address.hash(&mut hasher);
        ImageId(hasher.finish())
    }
}

/// Cached binary image with DWARF + unwind metadata.
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
    fn parse(desc: ImageDescriptor) -> Result<Self>
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

    /// Get the pointer size in bytes for this binary image
    ///
    /// Returns the size of a pointer (address) for the architecture
    /// this binary was compiled for. This is typically 8 bytes for
    /// 64-bit architectures (Arm64, X86_64) and 4 bytes for 32-bit architectures.
    ///
    /// ## Example
    ///
    /// ```no_run
    /// use ferros_core::symbols::SymbolCache;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let cache = SymbolCache::new();
    /// // After loading an image...
    /// // let image = cache.get_image(image_id)?;
    /// // let ptr_size = image.pointer_size();
    /// // println!("Pointer size: {} bytes", ptr_size);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
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

    pub fn id(&self) -> ImageId
    {
        self.id
    }

    pub fn path(&self) -> &Path
    {
        &self.path
    }

    pub fn architecture(&self) -> Architecture
    {
        self.architecture
    }

    pub fn contains(&self, address: Address) -> bool
    {
        let addr = address.value();
        addr >= self.runtime_range.0 && addr < self.runtime_range.1
    }

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
            // addr2line 0.25 uses gimli 0.32, compatible with our OwnedReader type
            let dwarf = Dwarf::load(|section| Ok::<_, gimli::Error>(self.section_reader(section)))
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to load DWARF for addr2line: {err}")))?;
            Context::from_dwarf(dwarf)
                .map_err(|err| DebuggerError::InvalidArgument(format!("failed to build addr2line context: {err}")))
        })
    }

    pub fn symbolicate(&self, address: Address) -> Option<Symbolication>
    {
        let file_addr = self.file_address(address)?;
        let ctx = self.symbol_context().ok()?;
        let mut frames = Vec::new();

        // addr2line 0.25 uses LookupResult API
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

/// Cached symbol metadata for a given address.
#[derive(Debug, Clone)]
pub struct Symbolication
{
    pub image_id: ImageId,
    pub frames: Vec<SymbolFrame>,
}

#[derive(Debug, Clone)]
pub struct SymbolFrame
{
    pub symbol: SymbolName,
    pub location: Option<SourceLocation>,
}

#[derive(Default)]
pub struct SymbolCache
{
    images: HashMap<ImageId, Arc<BinaryImage>>,
}

impl SymbolCache
{
    pub fn new() -> Self
    {
        Self { images: HashMap::new() }
    }

    pub fn load_image(&mut self, descriptor: ImageDescriptor) -> Result<Arc<BinaryImage>>
    {
        let canonical = if descriptor.path.is_absolute() {
            descriptor.path.clone()
        } else {
            descriptor.path.canonicalize().map_err(|err| {
                DebuggerError::InvalidArgument(format!("unable to canonicalize {}: {err}", descriptor.path.display()))
            })?
        };

        let id = ImageId::from_parts(&canonical, descriptor.load_address);
        if let Some(existing) = self.images.get(&id) {
            return Ok(existing.clone());
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let image = Arc::new(BinaryImage::parse(ImageDescriptor {
            path: canonical,
            load_address: descriptor.load_address,
        })?);
        self.images.insert(id, image.clone());
        Ok(image)
    }

    pub fn image_for_address(&self, address: Address) -> Option<Arc<BinaryImage>>
    {
        self.images.values().find(|image| image.contains(address)).cloned()
    }

    pub fn symbolicate(&self, address: Address) -> Option<Symbolication>
    {
        self.image_for_address(address).and_then(|image| image.symbolicate(address))
    }

    pub fn describe_type(&self, name: &str) -> Result<Option<Arc<TypeSummary>>>
    {
        for image in self.images.values() {
            if let Some(summary) = image.describe_type(name)? {
                return Ok(Some(summary));
            }
        }
        Ok(None)
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

fn make_symbol_name(raw: String) -> SymbolName
{
    let demangled = try_demangle(&raw).ok().map(|d| d.to_string());
    let language = if raw.starts_with("_R") || raw.starts_with("_ZN") || raw.contains("::") {
        SymbolLanguage::Rust
    } else if raw.starts_with("_Z") {
        SymbolLanguage::Cpp
    } else {
        SymbolLanguage::Unknown
    };

    SymbolName::new(raw, demangled, language)
}

/// Summary of a type extracted from DWARF.
#[derive(Debug, Clone)]
pub struct TypeSummary
{
    pub name: String,
    pub kind: TypeKind,
    pub size_bits: Option<u64>,
    pub fields: Vec<TypeField>,
    pub variants: Vec<TypeVariant>,
}

impl TypeSummary
{
    pub fn is_async_state_machine(&self) -> bool
    {
        if self.name.contains("::{{async}}") || self.name.contains("::{{generator}}") || self.name.contains("::{{opaque}}") {
            return true;
        }

        let state_field = self.fields.iter().any(|field| {
            matches!(
                field.name.as_deref(),
                Some("__state") | Some("__poll_state") | Some("__awaiter") | Some("__resume_state")
            )
        });
        let await_field = self
            .fields
            .iter()
            .any(|field| field.name.as_deref().map(|name| name.contains("await")).unwrap_or(false));
        let future_field = self.fields.iter().any(|field| {
            field
                .ty
                .as_deref()
                .map(|ty| ty.contains("core::future") || ty.contains("GenFuture"))
                .unwrap_or(false)
        });
        let variant_hint = self.variants.iter().any(|variant| {
            matches!(
                variant.name.as_deref(),
                Some("Pending") | Some("Ready") | Some("Complete") | Some("Terminated") | Some("Resolved")
            )
        });

        match self.kind {
            TypeKind::Struct | TypeKind::TraitObject => state_field && (await_field || future_field),
            TypeKind::Enum => variant_hint && (state_field || future_field),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind
{
    Struct,
    Enum,
    Union,
    TraitObject,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct TypeField
{
    pub name: Option<String>,
    pub ty: Option<String>,
    pub offset_bits: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TypeVariant
{
    pub name: Option<String>,
    pub discriminant: Option<i64>,
    pub fields: Vec<TypeField>,
}

const MAX_TYPE_REF_DEPTH: usize = 32;

struct TypeExtractor<'a>
{
    dwarf: &'a OwnedDwarf,
    units: Vec<Unit<OwnedReader>>,
}

impl<'a> TypeExtractor<'a>
{
    fn new(dwarf: &'a OwnedDwarf) -> Result<Self>
    {
        let mut units = Vec::new();
        let mut headers = dwarf.units();
        while let Some(header) = headers
            .next()
            .map_err(|err| map_dwarf_error("reading .debug_info unit header", err))?
        {
            units.push(
                dwarf
                    .unit(header)
                    .map_err(|err| map_dwarf_error("parsing compilation unit", err))?,
            );
        }

        let mut type_headers = dwarf.type_units();
        while let Some(header) = type_headers
            .next()
            .map_err(|err| map_dwarf_error("reading .debug_types unit header", err))?
        {
            units.push(dwarf.unit(header).map_err(|err| map_dwarf_error("parsing type unit", err))?);
        }

        Ok(Self { dwarf, units })
    }

    fn describe(&self, target: &str) -> Result<Option<TypeSummary>>
    {
        for unit in &self.units {
            if let Some(summary) = self.describe_in_unit(unit, target)? {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }

    fn describe_in_unit(&self, unit: &Unit<OwnedReader>, target: &str) -> Result<Option<TypeSummary>>
    {
        let mut cursor = unit.entries();
        while let Some((_delta, entry)) = cursor.next_dfs().map_err(|err| map_dwarf_error("traversing DIE tree", err))? {
            if !matches!(
                entry.tag(),
                constants::DW_TAG_structure_type
                    | constants::DW_TAG_class_type
                    | constants::DW_TAG_union_type
                    | constants::DW_TAG_enumeration_type
            ) {
                continue;
            }
            let Some(name) = self.entry_name(unit, entry)? else {
                continue;
            };
            if !Self::names_match(&name, target) {
                continue;
            }
            let summary = self.build_summary(unit, entry.clone(), name)?;
            return Ok(Some(summary));
        }
        Ok(None)
    }

    fn build_summary(
        &self,
        unit: &Unit<OwnedReader>,
        entry: DebuggingInformationEntry<'_, '_, OwnedReader>,
        name: String,
    ) -> Result<TypeSummary>
    {
        let mut kind = match entry.tag() {
            constants::DW_TAG_structure_type | constants::DW_TAG_class_type => TypeKind::Struct,
            constants::DW_TAG_union_type => TypeKind::Union,
            constants::DW_TAG_enumeration_type => TypeKind::Enum,
            _ => TypeKind::Unknown,
        };

        if matches!(entry.tag(), constants::DW_TAG_structure_type | constants::DW_TAG_class_type) && is_trait_object(&name) {
            kind = TypeKind::TraitObject;
        }

        let mut fields = Vec::new();
        let mut variants = Vec::new();

        match entry.tag() {
            constants::DW_TAG_structure_type | constants::DW_TAG_class_type => {
                let (struct_fields, struct_variants, has_variants) = self.collect_struct_members(unit, entry.offset())?;
                fields = struct_fields;
                variants = struct_variants;
                if has_variants {
                    kind = TypeKind::Enum;
                }
            }
            constants::DW_TAG_union_type => {
                fields = self.collect_union_members(unit, entry.offset())?;
            }
            constants::DW_TAG_enumeration_type => {
                variants = self.collect_enumerators(unit, entry.offset())?;
            }
            _ => {}
        }

        let size_bits = self.entry_size_bits(&entry)?;

        Ok(TypeSummary {
            name,
            kind,
            size_bits,
            fields,
            variants,
        })
    }

    fn entry_size_bits(&self, entry: &DebuggingInformationEntry<'_, '_, OwnedReader>) -> Result<Option<u64>>
    {
        if let Some(attr) = entry
            .attr(constants::DW_AT_bit_size)
            .map_err(|err| map_dwarf_error("reading DW_AT_bit_size", err))?
        {
            if let Some(bits) = attr.udata_value() {
                return Ok(Some(bits));
            }
        }

        if let Some(attr) = entry
            .attr(constants::DW_AT_byte_size)
            .map_err(|err| map_dwarf_error("reading DW_AT_byte_size", err))?
        {
            if let Some(bytes) = attr.udata_value() {
                return Ok(Some(bytes * 8));
            }
        }

        Ok(None)
    }

    fn collect_struct_members(
        &self,
        unit: &Unit<OwnedReader>,
        offset: UnitOffset<usize>,
    ) -> Result<(Vec<TypeField>, Vec<TypeVariant>, bool)>
    {
        let mut fields = Vec::new();
        let mut variants = Vec::new();
        let mut has_variant_part = false;

        let mut tree = unit
            .entries_tree(Some(offset))
            .map_err(|err| map_dwarf_error("building struct tree", err))?;
        let root = tree.root().map_err(|err| map_dwarf_error("navigating struct root", err))?;
        let mut children = root.children();
        while let Some(child) = children
            .next()
            .map_err(|err| map_dwarf_error("iterating struct children", err))?
        {
            let child_entry = child.entry().clone();
            match child_entry.tag() {
                constants::DW_TAG_member => fields.push(self.build_field(unit, &child_entry)?),
                constants::DW_TAG_variant_part => {
                    has_variant_part = true;
                    variants.extend(self.collect_variants_from_offset(unit, child_entry.offset())?);
                }
                _ => {}
            }
        }

        Ok((fields, variants, has_variant_part))
    }

    fn collect_union_members(&self, unit: &Unit<OwnedReader>, offset: UnitOffset<usize>) -> Result<Vec<TypeField>>
    {
        let mut fields = Vec::new();
        let mut tree = unit
            .entries_tree(Some(offset))
            .map_err(|err| map_dwarf_error("building union tree", err))?;
        let root = tree.root().map_err(|err| map_dwarf_error("navigating union root", err))?;
        let mut children = root.children();
        while let Some(child) = children
            .next()
            .map_err(|err| map_dwarf_error("iterating union children", err))?
        {
            let child_entry = child.entry().clone();
            if child_entry.tag() == constants::DW_TAG_member {
                fields.push(self.build_field(unit, &child_entry)?);
            }
        }
        Ok(fields)
    }

    fn collect_variants_from_offset(&self, unit: &Unit<OwnedReader>, offset: UnitOffset<usize>) -> Result<Vec<TypeVariant>>
    {
        let mut variants = Vec::new();
        let mut tree = unit
            .entries_tree(Some(offset))
            .map_err(|err| map_dwarf_error("building variant tree", err))?;
        let node = tree.root().map_err(|err| map_dwarf_error("navigating variant root", err))?;
        let mut children = node.children();
        while let Some(variant_node) = children.next().map_err(|err| map_dwarf_error("iterating variants", err))? {
            let entry = variant_node.entry().clone();
            if entry.tag() != constants::DW_TAG_variant {
                continue;
            }

            let mut field_iter = variant_node.children();
            let mut variant_fields = Vec::new();
            while let Some(field_node) = field_iter
                .next()
                .map_err(|err| map_dwarf_error("iterating variant fields", err))?
            {
                let field_entry = field_node.entry().clone();
                if field_entry.tag() == constants::DW_TAG_member {
                    variant_fields.push(self.build_field(unit, &field_entry)?);
                }
            }

            variants.push(TypeVariant {
                name: self.entry_name(unit, &entry)?,
                discriminant: self.attribute_to_i64(
                    entry
                        .attr(constants::DW_AT_discr_value)
                        .map_err(|err| map_dwarf_error("reading DW_AT_discr_value", err))?,
                )?,
                fields: variant_fields,
            });
        }
        Ok(variants)
    }

    fn collect_enumerators(&self, unit: &Unit<OwnedReader>, offset: UnitOffset<usize>) -> Result<Vec<TypeVariant>>
    {
        let mut variants = Vec::new();
        let mut tree = unit
            .entries_tree(Some(offset))
            .map_err(|err| map_dwarf_error("building enumeration tree", err))?;
        let root = tree
            .root()
            .map_err(|err| map_dwarf_error("navigating enumeration root", err))?;
        let mut children = root.children();
        while let Some(child) = children.next().map_err(|err| map_dwarf_error("iterating enumerators", err))? {
            let entry = child.entry().clone();
            if entry.tag() != constants::DW_TAG_enumerator {
                continue;
            }

            variants.push(TypeVariant {
                name: self.entry_name(unit, &entry)?,
                discriminant: self.attribute_to_i64(
                    entry
                        .attr(constants::DW_AT_const_value)
                        .map_err(|err| map_dwarf_error("reading DW_AT_const_value", err))?,
                )?,
                fields: Vec::new(),
            });
        }
        Ok(variants)
    }

    fn build_field(
        &self,
        unit: &Unit<OwnedReader>,
        entry: &DebuggingInformationEntry<'_, '_, OwnedReader>,
    ) -> Result<TypeField>
    {
        let name = if let Some(attr) = entry
            .attr(constants::DW_AT_name)
            .map_err(|err| map_dwarf_error("reading field name", err))?
        {
            Some(self.attr_to_string(unit, attr.value())?)
        } else {
            None
        };

        let ty = if let Some(attr) = entry
            .attr(constants::DW_AT_type)
            .map_err(|err| map_dwarf_error("reading field type", err))?
        {
            self.resolve_type_name(unit, attr.value(), 0)?
        } else {
            None
        };

        let offset_bits = self.field_offset_bits(entry)?;

        Ok(TypeField { name, ty, offset_bits })
    }

    fn field_offset_bits(&self, entry: &DebuggingInformationEntry<'_, '_, OwnedReader>) -> Result<Option<u64>>
    {
        if let Some(attr) = entry
            .attr(constants::DW_AT_data_bit_offset)
            .map_err(|err| map_dwarf_error("reading DW_AT_data_bit_offset", err))?
        {
            if let Some(bits) = attr.udata_value() {
                return Ok(Some(bits));
            }
        }

        if let Some(attr) = entry
            .attr(constants::DW_AT_data_member_location)
            .map_err(|err| map_dwarf_error("reading DW_AT_data_member_location", err))?
        {
            if let Some(bytes) = attr.udata_value() {
                return Ok(Some(bytes * 8));
            }
        }

        Ok(None)
    }

    fn attribute_to_i64(&self, attr: Option<Attribute<OwnedReader>>) -> Result<Option<i64>>
    {
        Ok(attr.and_then(|attribute| {
            attribute
                .sdata_value()
                .or_else(|| attribute.udata_value().map(|value| value as i64))
        }))
    }

    fn entry_name(
        &self,
        unit: &Unit<OwnedReader>,
        entry: &DebuggingInformationEntry<'_, '_, OwnedReader>,
    ) -> Result<Option<String>>
    {
        if let Some(attr) = entry
            .attr(constants::DW_AT_name)
            .map_err(|err| map_dwarf_error("reading DW_AT_name", err))?
        {
            return Ok(Some(self.attr_to_string(unit, attr.value())?));
        }
        if let Some(attr) = entry
            .attr(constants::DW_AT_linkage_name)
            .map_err(|err| map_dwarf_error("reading DW_AT_linkage_name", err))?
        {
            return Ok(Some(self.attr_to_string(unit, attr.value())?));
        }
        Ok(None)
    }

    fn attr_to_string(&self, unit: &Unit<OwnedReader>, value: AttributeValue<OwnedReader>) -> Result<String>
    {
        let reader = self
            .dwarf
            .attr_string(unit, value)
            .map_err(|err| map_dwarf_error("resolving DWARF string", err))?;
        let owned = match reader.to_string() {
            Ok(cow) => cow.into_owned(),
            Err(_) => reader
                .to_string_lossy()
                .map_err(|err| map_dwarf_error("decoding DWARF string", err))?
                .into_owned(),
        };
        Ok(owned)
    }

    fn resolve_type_name(
        &self,
        unit: &Unit<OwnedReader>,
        value: AttributeValue<OwnedReader>,
        depth: usize,
    ) -> Result<Option<String>>
    {
        if depth >= MAX_TYPE_REF_DEPTH {
            return Ok(None);
        }

        match value {
            AttributeValue::UnitRef(offset) => self.resolve_type_name_at_offset(unit, offset, depth + 1),
            AttributeValue::DebugInfoRef(offset) => {
                let target = UnitSectionOffset::from(offset);
                if let Some((target_unit, unit_offset)) = self.find_unit_for_offset(target) {
                    self.resolve_type_name_at_offset(target_unit, unit_offset, depth + 1)
                } else {
                    Ok(None)
                }
            }
            AttributeValue::DebugTypesRef(signature) => self.resolve_type_name_for_signature(signature, depth + 1),
            _ => Ok(None),
        }
    }

    fn resolve_type_name_at_offset(
        &self,
        unit: &Unit<OwnedReader>,
        offset: UnitOffset<usize>,
        depth: usize,
    ) -> Result<Option<String>>
    {
        let die = unit
            .entry(offset)
            .map_err(|err| map_dwarf_error("resolving type reference", err))?;
        if let Some(name) = self.entry_name(unit, &die)? {
            return Ok(Some(name));
        }
        if let Some(attr) = die
            .attr(constants::DW_AT_type)
            .map_err(|err| map_dwarf_error("reading nested type", err))?
        {
            let inner = self.resolve_type_name(unit, attr.value(), depth + 1)?;
            return Ok(inner);
        }
        Ok(None)
    }

    fn resolve_type_name_for_signature(&self, signature: DebugTypeSignature, depth: usize) -> Result<Option<String>>
    {
        for unit in &self.units {
            match unit.header.type_() {
                UnitType::Type {
                    type_signature,
                    type_offset,
                }
                | UnitType::SplitType {
                    type_signature,
                    type_offset,
                } if type_signature == signature => {
                    return self.resolve_type_name_at_offset(unit, type_offset, depth + 1);
                }
                _ => {}
            }
        }
        Ok(None)
    }

    fn find_unit_for_offset(&self, target: UnitSectionOffset<usize>) -> Option<(&Unit<OwnedReader>, UnitOffset<usize>)>
    {
        self.units
            .iter()
            .find_map(|unit| target.to_unit_offset(unit).map(|offset| (unit, offset)))
    }

    fn names_match(candidate: &str, wanted: &str) -> bool
    {
        candidate == wanted || candidate.strip_prefix("::") == Some(wanted) || wanted.strip_prefix("::") == Some(candidate)
    }
}

fn is_trait_object(name: &str) -> bool
{
    let trimmed = name.trim();
    trimmed.starts_with("dyn ") || trimmed.starts_with("(dyn ") || trimmed.contains(" as dyn ") || trimmed.contains(" dyn ")
}

fn map_dwarf_error(context: &str, err: gimli::Error) -> DebuggerError
{
    DebuggerError::InvalidArgument(format!("{context}: {err}"))
}
