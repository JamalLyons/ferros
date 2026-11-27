//! Types of a target program, read from an image's DWARF.
//!
//! A [`TypeTable`] belongs to one [`BinaryImage`](crate::symbols::BinaryImage) and is built
//! on the first call to [`BinaryImage::types`](crate::symbols::BinaryImage::types). Building
//! it walks every compilation unit once to index the qualified names of the type entries;
//! the types themselves are resolved lazily, the first time something asks for them:
//!
//! - by DIE offset: [`TypeTable::type_at`] for a type entry, [`TypeTable::type_of`] for the
//!   `DW_AT_type` of any entry (a variable, a parameter, a member);
//! - by qualified Rust name: [`TypeTable::type_named`], such as
//!   `core::option::Option<u32>` or `my_crate::shapes::Point`.
//!
//! Each resolved type gets a [`TypeId`] into the table's arena and keeps it for the life of
//! the table, so a second lookup of the same entry returns the same id. Resolving a type
//! also resolves every type it refers to (member types, pointees, elements, template
//! parameters), each once: an entry already in the arena, or already queued, is not read
//! again, so a recursive type (a linked-list node whose `next` points back at the node)
//! resolves to a finite graph whose pointee id is the node's own id.
//!
//! # What a type records
//!
//! [`Type`] carries the entry's qualified name, size, alignment and [`TypeKind`]: base
//! types, structs, tuples, unions, Rust enums (`DW_TAG_variant_part`: the discriminant
//! member and each variant's discriminant values), C-like enumerations, arrays with their
//! lengths, pointers and references with their pointee, typedefs, qualifiers and function
//! types. Members keep their names and byte offsets; structs and enums keep their
//! template type parameters (`T` of `Option<T>` and `Vec<T>`), so pretty-printers can find
//! element types. When an enum declares none, the table collects named parameters from
//! its variant payload structs, preferring the first parameter with a type or value.
//!
//! Discriminant values are raw: the unsigned bits of the discriminant member, at its size,
//! whatever the form DWARF used to encode them. A niche-encoded enum's dataful variant has
//! no value ([`Discriminant::Default`]); a list of values or ranges (`DW_AT_discr_list`) is
//! kept as [`DiscriminantRange`]s, which wrap like niche ranges do. Reading a value and
//! deciding which variant it is are the formatter's job.
//!
//! Names are qualified by the namespaces and types that enclose the entry, joined with
//! `::`. An array without a name is named `[T; N]` after its element, and an unnamed pointer
//! `*mut T` after its pointee, so arrays read the way Rust writes them.
//!
//! # Unreadable debug info
//!
//! Failures to read a unit or its entries are recorded in [`TypeTable::skipped`]. Names
//! found before the failure and in other readable units remain indexed. A type entry that
//! cannot be read when it is resolved becomes [`TypeKind::Unreadable`] with the reason, so a
//! walker shows it as such instead of failing the whole value. Type units (`.debug_types`)
//! are not read: a reference into them resolves to no type.
//!
//! # Map of this file
//!
//! - Identities: `DieOffset`, `TypeId`.
//! - The model: `Type`, `TypeKind`, `BaseEncoding`, `Composite`, `Member`, `TemplateParam`,
//!   `RustEnum`, `Variant`, `Discriminant`, `DiscriminantRange`, `Enumeration`,
//!   `Enumerator`, `ArrayType`, `PointerType`, `PointerKind`, `Qualifier`, `FunctionType`.
//! - Table storage: `IndexedUnit`, `Arena`, `TypeTable`.
//! - `TypeTable`: `build`, `skipped`, `len`, `is_empty`, `get`, `type_at`, `type_of`,
//!   `type_named`, `array_named`, `strip_aliases`, `unit_at`, `resolve`, `read_type`,
//!   `read_entry`, `composite`, `variant_part`, `member`, `scalar_info`.
//! - Name indexing: `index_unit`.
//! - Derived type metadata: `derive_name`, `derive_size`, `variant_template_params`, `unreadable`, `qualified`.
//! - Attribute helpers: `own_name`, `udata`, `flag`, `type_ref`, `die_ref`, `member_location`,
//!   `raw_bits`, `cut`, `discriminant_list`, `base_encoding`, `pointer_kind`, `is_type_tag`.
//! - Unit tests over synthetic DWARF in `mod tests`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, PoisonError, RwLock};

use gimli::{
    AttributeValue, DebuggingInformationEntry, DwTag, EntriesTreeNode, Reader as _, RunTimeEndian, UnitOffset, constants,
};

use crate::symbols::lines::SkippedDebugInfo;
use crate::symbols::{OwnedDwarf, OwnedReader};

type Unit = gimli::Unit<OwnedReader>;
type Entry<'abbrev, 'unit> = DebuggingInformationEntry<'abbrev, 'unit, OwnedReader>;
type Node<'abbrev, 'unit, 'tree> = EntriesTreeNode<'abbrev, 'unit, 'tree, OwnedReader>;

/// How many typedefs and qualifiers a size, name or alias lookup follows before giving up
/// (malformed DWARF can make them a cycle).
const ALIAS_DEPTH: usize = 16;

/// The offset of a debugging information entry in its image's `.debug_info` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DieOffset(pub u64);

/// A resolved type in one [`TypeTable`]. Ids are dense, start at zero and are never reused;
/// an id means nothing in another image's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(u32);

impl TypeId
{
    /// The id's position in its table's arena.
    #[must_use]
    pub const fn index(self) -> usize
    {
        self.0 as usize
    }
}

/// One type of the target program.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Type
{
    /// The entry the type was read from.
    pub die: DieOffset,
    /// Qualified name (`core::option::Option<u32>`, `&u16`, `(u8, u32)`), or a derived one
    /// for an unnamed array (`[u16; 4]`) or pointer (`*mut T`). `None` for other unnamed
    /// types.
    pub name: Option<String>,
    /// Size in bytes: `DW_AT_byte_size`, or for a pointer without one the unit's address
    /// size, for an array its element size times its lengths, for a typedef or qualifier
    /// its target's size.
    pub size: Option<u64>,
    /// Alignment in bytes (`DW_AT_alignment`), when DWARF records it.
    pub alignment: Option<u64>,
    /// Whether the entry is only a declaration (`DW_AT_declaration`): its members are
    /// described elsewhere, if anywhere.
    pub declaration: bool,
    /// What kind of type it is and what it is made of.
    pub kind: TypeKind,
}

/// The structure of a [`Type`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeKind
{
    /// A primitive (`DW_TAG_base_type`): integers, floats, `bool`, `char`, and `()`.
    Base(BaseEncoding),
    /// A struct or class (`DW_TAG_structure_type`, `DW_TAG_class_type`) without a variant
    /// part.
    Struct(Composite),
    /// A Rust tuple: a struct named `(..)` whose members are `__0`, `__1`, ...
    Tuple(Composite),
    /// A union (`DW_TAG_union_type`). Members without a location are at offset 0.
    Union(Composite),
    /// A Rust enum: a struct with a `DW_TAG_variant_part`.
    Enum(RustEnum),
    /// A C-like enumeration (`DW_TAG_enumeration_type`), which is also how rustc describes
    /// a Rust enum whose variants carry no data.
    Enumeration(Enumeration),
    /// An array (`DW_TAG_array_type`).
    Array(ArrayType),
    /// A reference, raw pointer, `Box` or other thin pointer (`DW_TAG_pointer_type`,
    /// `DW_TAG_reference_type`, `DW_TAG_rvalue_reference_type`). Fat pointers (`&[T]`,
    /// `&str`, `&dyn Trait`) are structs.
    Pointer(PointerType),
    /// Another name for `target` (`DW_TAG_typedef`).
    Typedef(Option<TypeId>),
    /// `target` with a qualifier (`const`, `volatile`, ...).
    Qualified
    {
        /// Which qualifier.
        qualifier: Qualifier,
        /// The qualified type; `None` for a qualified `void`.
        target: Option<TypeId>,
    },
    /// A function type (`DW_TAG_subroutine_type`), the pointee of a function pointer.
    Function(FunctionType),
    /// A type DWARF names but does not describe (`DW_TAG_unspecified_type`).
    Unspecified,
    /// A type entry with a tag this model does not describe.
    Unsupported(u16),
    /// The entry could not be read; the string says why.
    Unreadable(String),
}

/// How a base type's bytes encode its value (`DW_AT_encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BaseEncoding
{
    /// Two's complement integer.
    Signed,
    /// Unsigned integer (also `()`, which rustc describes as a zero-sized unsigned type).
    Unsigned,
    /// IEEE floating point.
    Float,
    /// `bool`.
    Boolean,
    /// A Unicode scalar value (`char`, `DW_ATE_UTF`).
    Char,
    /// A C `signed char`.
    SignedChar,
    /// A C `unsigned char`.
    UnsignedChar,
    /// Another `DW_ATE_*` value, or none.
    Other(u8),
}

/// The members of a struct, tuple or union.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Composite
{
    /// Data members, in DWARF order (declaration order, not layout order).
    pub members: Vec<Member>,
    /// Generic parameters (`DW_TAG_template_type_parameter`, `DW_TAG_template_value_parameter`).
    pub template_params: Vec<TemplateParam>,
}

impl Composite
{
    /// The member named `name`.
    #[must_use]
    pub fn member(&self, name: &str) -> Option<&Member>
    {
        self.members.iter().find(|member| member.name.as_deref() == Some(name))
    }

    /// The type parameter named `name` (`T` of `Option<T>`).
    #[must_use]
    pub fn template_param(&self, name: &str) -> Option<&TemplateParam>
    {
        self.template_params.iter().find(|param| param.name.as_deref() == Some(name))
    }
}

/// One data member (`DW_TAG_member`, or a base class through `DW_TAG_inheritance`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Member
{
    /// The member's name (`x`, or `__0` for a tuple field); `None` for an anonymous member
    /// or a discriminant.
    pub name: Option<String>,
    /// The member's type.
    pub ty: Option<TypeId>,
    /// Byte offset from the start of the containing type (`DW_AT_data_member_location`).
    pub offset: Option<u64>,
    /// Bit offset from the start of the containing type, for a bit field
    /// (`DW_AT_data_bit_offset`).
    pub bit_offset: Option<u64>,
    /// Width in bits, for a bit field (`DW_AT_bit_size`).
    pub bit_size: Option<u64>,
    /// Whether the compiler made the member up (`DW_AT_artificial`), as rustc does for an
    /// enum's discriminant.
    pub artificial: bool,
}

/// A generic parameter of a type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TemplateParam
{
    /// The parameter's name (`T`).
    pub name: Option<String>,
    /// The type argument, or the type of a value argument.
    pub ty: Option<TypeId>,
    /// The raw bits of a value argument (a const generic), when it has one.
    pub value: Option<u128>,
}

/// A Rust enum: the struct rustc emits around a `DW_TAG_variant_part`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RustEnum
{
    /// Members of the struct outside the variant part (rustc emits none).
    pub members: Vec<Member>,
    /// Generic parameters of the enum, inherited from its payload structs if absent here.
    pub template_params: Vec<TemplateParam>,
    /// The member `DW_AT_discr` names, whose bits select the variant: an explicit tag, or
    /// the niche of a niche-encoded enum. `None` for an enum with one variant, which needs
    /// no discriminant.
    pub discriminant: Option<Member>,
    /// The variants, in DWARF order.
    pub variants: Vec<Variant>,
}

impl RustEnum
{
    /// The generic parameter named `name`, if DWARF describes one.
    #[must_use]
    pub fn template_param(&self, name: &str) -> Option<&TemplateParam>
    {
        self.template_params.iter().find(|param| param.name.as_deref() == Some(name))
    }

    /// The variant named `name`.
    #[must_use]
    pub fn variant(&self, name: &str) -> Option<&Variant>
    {
        self.variants.iter().find(|variant| variant.name.as_deref() == Some(name))
    }

    /// The variant a discriminant whose raw bits are `raw` selects: the first variant whose
    /// values contain it, else the default variant.
    #[must_use]
    pub fn variant_for(&self, raw: u128) -> Option<&Variant>
    {
        self.variants
            .iter()
            .find(|variant| variant.discriminant.contains(raw))
            .or_else(|| {
                self.variants
                    .iter()
                    .find(|variant| variant.discriminant == Discriminant::Default)
            })
    }
}

/// One variant of a [`RustEnum`] (`DW_TAG_variant`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Variant
{
    /// The variant's name: the name of its first named member (`Some`, `Circle`).
    pub name: Option<String>,
    /// The discriminant values that select this variant.
    pub discriminant: Discriminant,
    /// The variant's members. rustc emits one, named after the variant, whose type is a
    /// struct holding the variant's fields at their offsets in the whole enum.
    pub members: Vec<Member>,
}

/// The discriminant values that select a [`Variant`], as raw bits of the discriminant
/// member at its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discriminant
{
    /// No values: the variant every value no other variant claims selects (the dataful
    /// variant of a niche-encoded enum).
    Default,
    /// The listed values (`DW_AT_discr_value`: one value, or `DW_AT_discr_list`: values and
    /// ranges). Empty when DWARF encodes them in a form this model does not read.
    Values(Vec<DiscriminantRange>),
}

impl Discriminant
{
    /// Whether these values include `raw`. [`Default`](Self::Default) contains nothing; see
    /// [`RustEnum::variant_for`].
    #[must_use]
    pub fn contains(&self, raw: u128) -> bool
    {
        match self {
            Self::Default => false,
            Self::Values(ranges) => ranges.iter().any(|range| range.contains(raw)),
        }
    }
}

/// An inclusive range of raw discriminant values. A range whose `low` is above its `high`
/// wraps around the discriminant's width, as a niche range can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiscriminantRange
{
    /// The first value.
    pub low: u128,
    /// The last value.
    pub high: u128,
}

impl DiscriminantRange
{
    /// The range holding `value` alone.
    #[must_use]
    pub const fn single(value: u128) -> Self
    {
        Self { low: value, high: value }
    }

    /// Whether `raw` is in the range.
    #[must_use]
    pub const fn contains(&self, raw: u128) -> bool
    {
        if self.low <= self.high {
            self.low <= raw && raw <= self.high
        } else {
            raw >= self.low || raw <= self.high
        }
    }
}

/// A C-like enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Enumeration
{
    /// The integer type that holds the value.
    pub underlying: Option<TypeId>,
    /// The named values, in DWARF order.
    pub enumerators: Vec<Enumerator>,
}

/// One named value of an [`Enumeration`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Enumerator
{
    /// The value's name.
    pub name: Option<String>,
    /// The value's raw bits at the enumeration's size.
    pub value: Option<u128>,
}

/// An array type.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ArrayType
{
    /// The element type.
    pub element: Option<TypeId>,
    /// The length of each dimension, outermost first; `None` for a dimension whose length
    /// DWARF does not give as a constant. Rust arrays have one dimension.
    pub lengths: Vec<Option<u64>>,
}

/// A thin pointer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PointerType
{
    /// What kind of pointer, from the tag and the Rust name.
    pub kind: PointerKind,
    /// The pointed-to type; `None` for a pointer to `void`.
    pub pointee: Option<TypeId>,
}

/// What a [`PointerType`] is in Rust terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PointerKind
{
    /// `&T` or `&mut T` (and C++ references, as mutable).
    Reference
    {
        /// `&mut`.
        mutable: bool,
    },
    /// `*const T` or `*mut T` (and unnamed pointers, as `*mut`).
    Raw
    {
        /// `*mut`.
        mutable: bool,
    },
    /// `Box<T>`, which rustc describes as a pointer named after the box.
    Box,
    /// Another named pointer, such as a function pointer (`fn(u32) -> u32`).
    Other,
}

/// A type qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Qualifier
{
    /// `DW_TAG_const_type`.
    Const,
    /// `DW_TAG_volatile_type`.
    Volatile,
    /// `DW_TAG_restrict_type`.
    Restrict,
    /// `DW_TAG_atomic_type`.
    Atomic,
    /// `DW_TAG_immutable_type`.
    Immutable,
}

/// A function type.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct FunctionType
{
    /// The return type; `None` for a function that returns nothing.
    pub return_type: Option<TypeId>,
    /// The parameter types, in order.
    pub parameters: Vec<Option<TypeId>>,
}

/// One compilation unit and the `.debug_info` range it covers.
struct IndexedUnit
{
    start: usize,
    end: usize,
    unit: Unit,
}

/// The resolved types and the entry each came from.
#[derive(Default)]
struct Arena
{
    types: Vec<Arc<Type>>,
    by_die: HashMap<usize, TypeId>,
}

/// The types of one image, resolved on demand (see the module documentation).
pub struct TypeTable
{
    dwarf: OwnedDwarf,
    endian: RunTimeEndian,
    /// Sorted by `start`.
    units: Vec<IndexedUnit>,
    /// Qualified name of every named type entry, by `.debug_info` offset.
    names: HashMap<usize, String>,
    /// Type entries by qualified name: definitions in offset order, then declarations.
    by_name: HashMap<String, Vec<usize>>,
    /// `(element entry, array entry)` of every unnamed array entry, in offset order: rustc
    /// names no array, so [`type_named`](Self::type_named) finds `[T; N]` through these.
    arrays: Vec<(usize, usize)>,
    skipped: Vec<SkippedDebugInfo>,
    arena: RwLock<Arena>,
}

impl std::fmt::Debug for TypeTable
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        f.debug_struct("TypeTable")
            .field("units", &self.units.len())
            .field("names", &self.by_name.len())
            .field("resolved", &self.len())
            .field("skipped", &self.skipped)
            .finish_non_exhaustive()
    }
}

impl TypeTable
{
    /// Index the type names of every compilation unit of `dwarf` that can be parsed.
    pub(crate) fn build(dwarf: OwnedDwarf, endian: RunTimeEndian) -> Self
    {
        let mut units = Vec::new();
        let mut names = HashMap::new();
        let mut by_name = HashMap::<String, Vec<(bool, usize)>>::new();
        let mut arrays = Vec::new();
        let mut skipped = Vec::new();
        let mut headers = dwarf.units();
        loop {
            let header = match headers.next() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                Err(error) => {
                    skipped.push(SkippedDebugInfo::new(None, "unit header", error));
                    break;
                }
            };
            let Some(start) = header.offset().as_debug_info_offset().map(|offset| offset.0) else {
                continue;
            };
            let end = start + header.length_including_self();
            let unit = match dwarf.unit(header) {
                Ok(unit) => unit,
                Err(error) => {
                    skipped.push(SkippedDebugInfo::new(Some(start as u64), "unit", error));
                    continue;
                }
            };
            index_unit(&dwarf, &unit, start, &mut names, &mut by_name, &mut arrays, &mut skipped);
            units.push(IndexedUnit { start, end, unit });
        }
        units.sort_by_key(|unit| unit.start);
        let by_name = by_name
            .into_iter()
            .map(|(name, mut entries)| {
                entries.sort_unstable();
                (name, entries.into_iter().map(|(_, offset)| offset).collect())
            })
            .collect();
        Self {
            dwarf,
            endian,
            units,
            names,
            by_name,
            arrays,
            skipped,
            arena: RwLock::new(Arena::default()),
        }
    }

    /// Debug info left out of the name index because it could not be parsed.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedDebugInfo]
    {
        &self.skipped
    }

    /// How many types have been resolved so far.
    #[must_use]
    pub fn len(&self) -> usize
    {
        self.arena.read().unwrap_or_else(PoisonError::into_inner).types.len()
    }

    /// Whether no type has been resolved yet.
    #[must_use]
    pub fn is_empty(&self) -> bool
    {
        self.len() == 0
    }

    /// The type at `id`'s arena index. Pass an id obtained from this table.
    #[must_use]
    pub fn get(&self, id: TypeId) -> Option<Arc<Type>>
    {
        let arena = self.arena.read().unwrap_or_else(PoisonError::into_inner);
        arena.types.get(id.index()).cloned()
    }

    /// The type described by the type entry at `die`, resolving it on first use. `None`
    /// when no unit holds `die` or the entry there is not a type.
    #[must_use]
    pub fn type_at(&self, die: DieOffset) -> Option<TypeId>
    {
        let offset = usize::try_from(die.0).ok()?;
        let unit = self.unit_at(offset)?;
        let entry = unit.unit.entry(UnitOffset(offset - unit.start)).ok()?;
        is_type_tag(entry.tag()).then(|| self.resolve(offset))
    }

    /// The type of the entry at `die` (its `DW_AT_type`): a variable's, a parameter's or a
    /// member's type. `None` when the entry cannot be read or has no type.
    #[must_use]
    pub fn type_of(&self, die: DieOffset) -> Option<TypeId>
    {
        let offset = usize::try_from(die.0).ok()?;
        let unit = self.unit_at(offset)?;
        let entry = unit.unit.entry(UnitOffset(offset - unit.start)).ok()?;
        let target = type_ref(&entry, unit.start).ok()??;
        self.type_at(DieOffset(target as u64))
    }

    /// The type whose qualified name is `name` (a leading `::` is ignored): the definition
    /// at the lowest offset, else a declaration. rustc repeats a type in every unit that
    /// uses it; any copy describes the same layout. An array, which DWARF does not name, is
    /// found by its derived Rust spelling `[T; N]`.
    #[must_use]
    pub fn type_named(&self, name: &str) -> Option<TypeId>
    {
        let name = name.trim();
        let name = name.strip_prefix("::").unwrap_or(name);
        if let Some(&offset) = self.by_name.get(name).and_then(|entries| entries.first()) {
            return Some(self.resolve(offset));
        }
        self.array_named(name)
    }

    /// The first unnamed array entry spelled `[element; length]`.
    fn array_named(&self, name: &str) -> Option<TypeId>
    {
        let (element, length) = name.strip_prefix('[')?.strip_suffix(']')?.rsplit_once(';')?;
        let element = element.trim();
        let element = element.strip_prefix("::").unwrap_or(element);
        let length: u64 = length.trim().parse().ok()?;
        let spelling = format!("[{element}; {length}]");
        self.arrays
            .iter()
            .filter(|&&(element_offset, _)| {
                element.starts_with('[') || self.names.get(&element_offset).is_none_or(|found| found == element)
            })
            .find_map(|&(_, offset)| {
                let id = self.resolve(offset);
                (self.get(id)?.name.as_deref() == Some(spelling.as_str())).then_some(id)
            })
    }

    /// `id` with typedefs and qualifiers removed: the type a value of `id` is laid out as.
    #[must_use]
    pub fn strip_aliases(&self, id: TypeId) -> TypeId
    {
        let mut current = id;
        for _ in 0..ALIAS_DEPTH {
            let Some(ty) = self.get(current) else {
                break;
            };
            match ty.kind {
                TypeKind::Typedef(Some(target))
                | TypeKind::Qualified {
                    target: Some(target), ..
                } => current = target,
                _ => break,
            }
        }
        current
    }

    fn unit_at(&self, offset: usize) -> Option<&IndexedUnit>
    {
        let after = self.units.partition_point(|unit| unit.start <= offset);
        let unit = &self.units[after.checked_sub(1)?];
        (offset < unit.end).then_some(unit)
    }

    /// The id of the type entry at `offset`, reading it and every type it refers to that is
    /// not resolved yet.
    fn resolve(&self, offset: usize) -> TypeId
    {
        if let Some(&id) = self.arena.read().unwrap_or_else(PoisonError::into_inner).by_die.get(&offset) {
            return id;
        }
        let mut arena = self.arena.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(&id) = arena.by_die.get(&offset) {
            return id;
        }
        let base = arena.types.len();
        let mut new_ids = HashMap::<usize, TypeId>::new();
        let mut queue = VecDeque::new();
        let intern = |offset: usize, new_ids: &mut HashMap<usize, TypeId>, queue: &mut VecDeque<(TypeId, usize)>| {
            if let Some(&id) = arena.by_die.get(&offset) {
                return id;
            }
            let next = TypeId(u32::try_from(base + new_ids.len()).expect("fewer than 2^32 types"));
            *new_ids.entry(offset).or_insert_with(|| {
                queue.push_back((next, offset));
                next
            })
        };
        let root = intern(offset, &mut new_ids, &mut queue);
        let mut pending = Vec::<Type>::new();
        while let Some((id, offset)) = queue.pop_front() {
            debug_assert_eq!(id.index(), base + pending.len());
            let ty = self.read_type(offset, &mut |target| intern(target, &mut new_ids, &mut queue));
            pending.push(ty);
        }
        let lookup = |id: TypeId| -> Option<&Type> {
            match id.index().checked_sub(base) {
                Some(at) => pending.get(at),
                None => arena.types.get(id.index()).map(Arc::as_ref),
            }
        };
        let derived: Vec<_> = pending
            .iter()
            .enumerate()
            .map(|(at, ty)| {
                let id = TypeId((base + at) as u32);
                let params = match &ty.kind {
                    TypeKind::Enum(value) if value.template_params.is_empty() => {
                        Some(variant_template_params(&lookup, &value.variants))
                    }
                    _ => None,
                };
                (
                    derive_name(&lookup, id, ALIAS_DEPTH),
                    derive_size(&lookup, id, ALIAS_DEPTH),
                    params,
                )
            })
            .collect();
        for (ty, (name, size, params)) in pending.iter_mut().zip(derived) {
            ty.name = name;
            ty.size = size;
            if let (TypeKind::Enum(value), Some(params)) = (&mut ty.kind, params) {
                value.template_params = params;
            }
        }
        arena.types.extend(pending.into_iter().map(Arc::new));
        arena.by_die.extend(new_ids);
        root
    }

    /// The type at `offset`, with references to other types interned through `intern`.
    fn read_type(&self, offset: usize, intern: &mut dyn FnMut(usize) -> TypeId) -> Type
    {
        let die = DieOffset(offset as u64);
        let Some(unit) = self.unit_at(offset) else {
            return unreadable(die, "no compilation unit holds this entry".to_owned());
        };
        self.read_entry(unit, offset, intern)
            .unwrap_or_else(|error| unreadable(die, error.to_string()))
    }

    fn read_entry(&self, unit: &IndexedUnit, offset: usize, intern: &mut dyn FnMut(usize) -> TypeId) -> gimli::Result<Type>
    {
        let mut tree = unit.unit.entries_tree(Some(UnitOffset(offset - unit.start)))?;
        let root = tree.root()?;
        let entry = root.entry().clone();
        let tag = entry.tag();
        let name = match self.names.get(&offset) {
            Some(name) => Some(name.clone()),
            None => own_name(&self.dwarf, &unit.unit, &entry)?,
        };
        let mut size = udata(&entry, constants::DW_AT_byte_size)?;
        let target = type_ref(&entry, unit.start)?.map(&mut *intern);
        let kind = match tag {
            constants::DW_TAG_base_type => TypeKind::Base(base_encoding(&entry)?),
            constants::DW_TAG_structure_type | constants::DW_TAG_class_type | constants::DW_TAG_union_type => {
                self.composite(unit, root, tag, name.as_deref(), intern)?
            }
            constants::DW_TAG_enumeration_type => {
                let mut enumerators = Vec::new();
                let mut children = root.children();
                while let Some(child) = children.next()? {
                    let child = child.entry();
                    if child.tag() == constants::DW_TAG_enumerator {
                        enumerators.push(Enumerator {
                            name: own_name(&self.dwarf, &unit.unit, child)?,
                            value: child
                                .attr_value(constants::DW_AT_const_value)?
                                .and_then(|value| raw_bits(&value, size, self.endian)),
                        });
                    }
                }
                TypeKind::Enumeration(Enumeration {
                    underlying: target,
                    enumerators,
                })
            }
            constants::DW_TAG_array_type => {
                let mut lengths = Vec::new();
                let mut children = root.children();
                while let Some(child) = children.next()? {
                    let child = child.entry();
                    if matches!(
                        child.tag(),
                        constants::DW_TAG_subrange_type | constants::DW_TAG_enumeration_type
                    ) {
                        let count = match udata(child, constants::DW_AT_count)? {
                            Some(count) => Some(count),
                            None => {
                                let lower = udata(child, constants::DW_AT_lower_bound)?.unwrap_or(0);
                                udata(child, constants::DW_AT_upper_bound)?
                                    .and_then(|upper| upper.checked_sub(lower))
                                    .and_then(|span| span.checked_add(1))
                            }
                        };
                        lengths.push(count);
                    }
                }
                TypeKind::Array(ArrayType {
                    element: target,
                    lengths,
                })
            }
            constants::DW_TAG_pointer_type | constants::DW_TAG_reference_type | constants::DW_TAG_rvalue_reference_type => {
                size = size.or(Some(u64::from(unit.unit.header.address_size())));
                TypeKind::Pointer(PointerType {
                    kind: pointer_kind(tag, name.as_deref()),
                    pointee: target,
                })
            }
            constants::DW_TAG_typedef => TypeKind::Typedef(target),
            constants::DW_TAG_const_type => qualified(Qualifier::Const, target),
            constants::DW_TAG_volatile_type => qualified(Qualifier::Volatile, target),
            constants::DW_TAG_restrict_type => qualified(Qualifier::Restrict, target),
            constants::DW_TAG_atomic_type => qualified(Qualifier::Atomic, target),
            constants::DW_TAG_immutable_type => qualified(Qualifier::Immutable, target),
            constants::DW_TAG_subroutine_type => {
                let mut parameters = Vec::new();
                let mut children = root.children();
                while let Some(child) = children.next()? {
                    let child = child.entry();
                    if child.tag() == constants::DW_TAG_formal_parameter {
                        parameters.push(type_ref(child, unit.start)?.map(&mut *intern));
                    }
                }
                TypeKind::Function(FunctionType {
                    return_type: target,
                    parameters,
                })
            }
            constants::DW_TAG_unspecified_type => TypeKind::Unspecified,
            other => TypeKind::Unsupported(other.0),
        };
        Ok(Type {
            die: DieOffset(offset as u64),
            name,
            size,
            alignment: udata(&entry, constants::DW_AT_alignment)?,
            declaration: flag(&entry, constants::DW_AT_declaration)?,
            kind,
        })
    }

    /// A struct, class or union: a [`TypeKind::Enum`] when it has a variant part.
    fn composite(
        &self,
        unit: &IndexedUnit,
        root: Node<'_, '_, '_>,
        tag: DwTag,
        name: Option<&str>,
        intern: &mut dyn FnMut(usize) -> TypeId,
    ) -> gimli::Result<TypeKind>
    {
        let mut members = Vec::new();
        let mut template_params = Vec::new();
        let mut variant_part = None;
        let mut children = root.children();
        while let Some(child) = children.next()? {
            let entry = child.entry();
            match entry.tag() {
                constants::DW_TAG_member | constants::DW_TAG_inheritance => {
                    // A declared member is a static (C++), which takes no space here.
                    if !flag(entry, constants::DW_AT_declaration)? {
                        members.push(self.member(unit, entry, intern)?);
                    }
                }
                constants::DW_TAG_template_type_parameter | constants::DW_TAG_template_value_parameter => {
                    let ty = type_ref(entry, unit.start)?;
                    let value = match entry.attr_value(constants::DW_AT_const_value)? {
                        Some(value) => {
                            let size = ty.and_then(|ty| self.scalar_info(ty).0);
                            raw_bits(&value, size, self.endian)
                        }
                        None => None,
                    };
                    template_params.push(TemplateParam {
                        name: own_name(&self.dwarf, &unit.unit, entry)?,
                        ty: ty.map(&mut *intern),
                        value,
                    });
                }
                constants::DW_TAG_variant_part if variant_part.is_none() => {
                    variant_part = Some(self.variant_part(unit, child, intern)?);
                }
                _ => {}
            }
        }
        if let Some((discriminant, variants)) = variant_part {
            return Ok(TypeKind::Enum(RustEnum {
                members,
                template_params,
                discriminant,
                variants,
            }));
        }
        if tag == constants::DW_TAG_union_type {
            for member in &mut members {
                if member.offset.is_none() && member.bit_offset.is_none() {
                    member.offset = Some(0);
                }
            }
            return Ok(TypeKind::Union(Composite {
                members,
                template_params,
            }));
        }
        let composite = Composite {
            members,
            template_params,
        };
        Ok(match name {
            Some(name) if name.starts_with('(') && name.ends_with(')') => TypeKind::Tuple(composite),
            _ => TypeKind::Struct(composite),
        })
    }

    /// The discriminant member and the variants of a `DW_TAG_variant_part`.
    fn variant_part(
        &self,
        unit: &IndexedUnit,
        node: Node<'_, '_, '_>,
        intern: &mut dyn FnMut(usize) -> TypeId,
    ) -> gimli::Result<(Option<Member>, Vec<Variant>)>
    {
        let discr = die_ref(node.entry().attr_value(constants::DW_AT_discr)?, unit.start)?;
        let mut discriminant = None;
        // (size, signed) of the discriminant member's type.
        let mut info = (None, false);
        // (DW_AT_discr_value, DW_AT_discr_list, members) of each variant.
        let mut raw_variants = Vec::new();
        let mut children = node.children();
        while let Some(child) = children.next()? {
            match child.entry().tag() {
                constants::DW_TAG_member
                    if unit
                        .start
                        .checked_add(child.entry().offset().0)
                        .is_some_and(|offset| discr == Some(offset)) =>
                {
                    let entry = child.entry();
                    if let Some(ty) = type_ref(entry, unit.start)? {
                        info = self.scalar_info(ty);
                    }
                    discriminant = Some(self.member(unit, entry, intern)?);
                }
                constants::DW_TAG_variant => {
                    let entry = child.entry().clone();
                    let mut members = Vec::new();
                    let mut fields = child.children();
                    while let Some(field) = fields.next()? {
                        let field = field.entry();
                        if field.tag() == constants::DW_TAG_member {
                            members.push(self.member(unit, field, intern)?);
                        }
                    }
                    raw_variants.push((
                        entry.attr_value(constants::DW_AT_discr_value)?,
                        entry.attr_value(constants::DW_AT_discr_list)?,
                        members,
                    ));
                }
                _ => {}
            }
        }
        let (size, signed) = info;
        let variants = raw_variants
            .into_iter()
            .map(|(value, list, members)| {
                let discriminant = match (value, list) {
                    (Some(value), _) => Discriminant::Values(
                        raw_bits(&value, size, self.endian)
                            .map(DiscriminantRange::single)
                            .into_iter()
                            .collect(),
                    ),
                    (None, Some(list)) => Discriminant::Values(discriminant_list(&list, size, signed).unwrap_or_default()),
                    (None, None) => Discriminant::Default,
                };
                Variant {
                    name: members.iter().find_map(|member| member.name.clone()),
                    discriminant,
                    members,
                }
            })
            .collect();
        Ok((discriminant, variants))
    }

    fn member(
        &self,
        unit: &IndexedUnit,
        entry: &Entry<'_, '_>,
        intern: &mut dyn FnMut(usize) -> TypeId,
    ) -> gimli::Result<Member>
    {
        Ok(Member {
            name: own_name(&self.dwarf, &unit.unit, entry)?,
            ty: type_ref(entry, unit.start)?.map(intern),
            offset: member_location(entry, &unit.unit)?,
            bit_offset: udata(entry, constants::DW_AT_data_bit_offset)?,
            bit_size: udata(entry, constants::DW_AT_bit_size)?,
            artificial: flag(entry, constants::DW_AT_artificial)?,
        })
    }

    /// `(size, signed)` of the scalar type entry at `offset`, through typedefs, qualifiers
    /// and an enumeration's underlying type, without resolving it.
    fn scalar_info(&self, offset: usize) -> (Option<u64>, bool)
    {
        let mut size = None;
        let mut next = Some(offset);
        for _ in 0..ALIAS_DEPTH {
            let Some(offset) = next else {
                break;
            };
            let Some(unit) = self.unit_at(offset) else {
                break;
            };
            let Ok(entry) = unit.unit.entry(UnitOffset(offset - unit.start)) else {
                break;
            };
            size = size.or(udata(&entry, constants::DW_AT_byte_size).ok().flatten());
            if entry.tag() == constants::DW_TAG_base_type {
                let signed = matches!(base_encoding(&entry), Ok(BaseEncoding::Signed | BaseEncoding::SignedChar));
                return (size, signed);
            }
            next = type_ref(&entry, unit.start).ok().flatten();
        }
        (size, false)
    }
}

/// Record the qualified name of every named type entry of `unit`.
fn index_unit(
    dwarf: &OwnedDwarf,
    unit: &Unit,
    start: usize,
    names: &mut HashMap<usize, String>,
    by_name: &mut HashMap<String, Vec<(bool, usize)>>,
    arrays: &mut Vec<(usize, usize)>,
    skipped: &mut Vec<SkippedDebugInfo>,
)
{
    let unit_offset = Some(start as u64);
    let mut scopes = Vec::<Option<String>>::new();
    let mut depth = 0_isize;
    let mut entries = unit.entries();
    loop {
        let (delta, entry) = match entries.next_dfs() {
            Ok(Some(next)) => next,
            Ok(None) => break,
            Err(error) => {
                skipped.push(SkippedDebugInfo::new(unit_offset, "DIE tree", error));
                break;
            }
        };
        depth += delta;
        scopes.truncate(usize::try_from(depth).unwrap_or(0));
        let offset = start + entry.offset().0;
        let tag = entry.tag();
        let name = match own_name(dwarf, unit, entry) {
            Ok(name) => name,
            Err(error) => {
                skipped.push(SkippedDebugInfo::new(
                    unit_offset,
                    &format!("DIE at .debug_info+{offset:#x}"),
                    error,
                ));
                scopes.push(None);
                continue;
            }
        };
        if tag == constants::DW_TAG_array_type
            && name.is_none()
            && let Ok(Some(element)) = type_ref(entry, start)
        {
            arrays.push((element, offset));
        }
        if is_type_tag(tag)
            && let Some(name) = &name
        {
            let mut qualified: String = scopes.iter().flatten().fold(String::new(), |mut path, scope| {
                path.push_str(scope);
                path.push_str("::");
                path
            });
            qualified.push_str(name);
            let declaration = flag(entry, constants::DW_AT_declaration).unwrap_or(false);
            by_name.entry(qualified.clone()).or_default().push((declaration, offset));
            names.insert(offset, qualified);
        }
        let scope = matches!(
            tag,
            constants::DW_TAG_namespace
                | constants::DW_TAG_structure_type
                | constants::DW_TAG_class_type
                | constants::DW_TAG_union_type
                | constants::DW_TAG_enumeration_type
        )
        .then_some(name)
        .flatten();
        scopes.push(scope);
    }
}

/// The name of `id` in `lookup`: its own, or one derived from its element or pointee.
fn derive_name<'a>(lookup: &impl Fn(TypeId) -> Option<&'a Type>, id: TypeId, depth: usize) -> Option<String>
{
    let ty = lookup(id)?;
    if ty.name.is_some() || depth == 0 {
        return ty.name.clone();
    }
    let inner = |inner: Option<TypeId>, fallback: &str| {
        inner
            .and_then(|inner| derive_name(lookup, inner, depth - 1))
            .unwrap_or_else(|| fallback.to_owned())
    };
    match &ty.kind {
        TypeKind::Array(array) => {
            let mut name = inner(array.element, "?");
            for length in array.lengths.iter().rev() {
                name = match length {
                    Some(length) => format!("[{name}; {length}]"),
                    None => format!("[{name}]"),
                };
            }
            Some(name)
        }
        TypeKind::Pointer(pointer) => {
            let pointee = inner(pointer.pointee, "()");
            Some(match pointer.kind {
                PointerKind::Reference { mutable: true } => format!("&mut {pointee}"),
                PointerKind::Reference { mutable: false } => format!("&{pointee}"),
                PointerKind::Raw { mutable: false } => format!("*const {pointee}"),
                PointerKind::Raw { mutable: true } | PointerKind::Box | PointerKind::Other => format!("*mut {pointee}"),
            })
        }
        _ => None,
    }
}

/// The size of `id` in `lookup`: its own, or one derived from its element or target.
fn derive_size<'a>(lookup: &impl Fn(TypeId) -> Option<&'a Type>, id: TypeId, depth: usize) -> Option<u64>
{
    let ty = lookup(id)?;
    if ty.size.is_some() || depth == 0 {
        return ty.size;
    }
    match &ty.kind {
        TypeKind::Array(array) => {
            let element = derive_size(lookup, array.element?, depth - 1)?;
            array
                .lengths
                .iter()
                .try_fold(element, |size, length| size.checked_mul((*length)?))
        }
        TypeKind::Typedef(target) | TypeKind::Qualified { target, .. } => derive_size(lookup, (*target)?, depth - 1),
        _ => None,
    }
}

/// Collect named parameters from the payload structs in variant order.
fn variant_template_params<'a>(lookup: &impl Fn(TypeId) -> Option<&'a Type>, variants: &[Variant]) -> Vec<TemplateParam>
{
    let payload_params = variants
        .iter()
        .flat_map(|variant| &variant.members)
        .filter_map(|member| match &lookup(member.ty?)?.kind {
            TypeKind::Struct(payload) => Some(&payload.template_params),
            _ => None,
        })
        .flatten();
    let mut params = Vec::<TemplateParam>::new();
    for param in payload_params {
        let Some(name) = param.name.as_deref().filter(|name| !name.is_empty()) else {
            continue;
        };
        if let Some(previous) = params.iter_mut().find(|previous| previous.name.as_deref() == Some(name)) {
            if previous.ty.is_none() && previous.value.is_none() && (param.ty.is_some() || param.value.is_some()) {
                *previous = param.clone();
            }
        } else {
            params.push(param.clone());
        }
    }
    params
}

fn unreadable(die: DieOffset, reason: String) -> Type
{
    Type {
        die,
        name: None,
        size: None,
        alignment: None,
        declaration: false,
        kind: TypeKind::Unreadable(reason),
    }
}

const fn qualified(qualifier: Qualifier, target: Option<TypeId>) -> TypeKind
{
    TypeKind::Qualified { qualifier, target }
}

/// The entry's own `DW_AT_name`.
fn own_name(dwarf: &OwnedDwarf, unit: &Unit, entry: &Entry<'_, '_>) -> gimli::Result<Option<String>>
{
    match entry.attr_value(constants::DW_AT_name)? {
        Some(value) => Ok(Some(dwarf.attr_string(unit, value)?.to_string_lossy()?.into_owned())),
        None => Ok(None),
    }
}

fn udata(entry: &Entry<'_, '_>, name: gimli::DwAt) -> gimli::Result<Option<u64>>
{
    Ok(entry.attr(name)?.and_then(|attr| attr.udata_value()))
}

fn flag(entry: &Entry<'_, '_>, name: gimli::DwAt) -> gimli::Result<bool>
{
    Ok(matches!(entry.attr_value(name)?, Some(AttributeValue::Flag(true))))
}

/// The `.debug_info` offset `DW_AT_type` refers to; `None` without one or for a reference
/// into a type unit.
fn type_ref(entry: &Entry<'_, '_>, start: usize) -> gimli::Result<Option<usize>>
{
    die_ref(entry.attr_value(constants::DW_AT_type)?, start)
}

fn die_ref(value: Option<AttributeValue<OwnedReader>>, start: usize) -> gimli::Result<Option<usize>>
{
    match value {
        Some(AttributeValue::UnitRef(offset)) => {
            start.checked_add(offset.0).map(Some).ok_or(gimli::Error::OffsetOutOfBounds)
        }
        Some(AttributeValue::DebugInfoRef(offset)) => Ok(Some(offset.0)),
        _ => Ok(None),
    }
}

/// A member's byte offset: a constant `DW_AT_data_member_location`, or the
/// `DW_OP_plus_uconst` expression older producers write.
fn member_location(entry: &Entry<'_, '_>, unit: &Unit) -> gimli::Result<Option<u64>>
{
    let Some(attr) = entry.attr(constants::DW_AT_data_member_location)? else {
        return Ok(None);
    };
    if let Some(offset) = attr.udata_value() {
        return Ok(Some(offset));
    }
    let Some(expression) = attr.exprloc_value() else {
        return Ok(None);
    };
    let mut operations = expression.operations(unit.encoding());
    let first = operations.next()?;
    let rest = operations.next()?;
    Ok(match (first, rest) {
        (Some(gimli::Operation::PlusConstant { value } | gimli::Operation::UnsignedConstant { value }), None) => Some(value),
        _ => None,
    })
}

/// The unsigned bits of a constant attribute, cut to `size` bytes when known.
fn raw_bits(value: &AttributeValue<OwnedReader>, size: Option<u64>, endian: RunTimeEndian) -> Option<u128>
{
    use gimli::Endianity as _;

    let raw = match value {
        AttributeValue::Data1(value) => u128::from(*value),
        AttributeValue::Data2(value) => u128::from(*value),
        AttributeValue::Data4(value) => u128::from(*value),
        AttributeValue::Data8(value) | AttributeValue::Udata(value) => u128::from(*value),
        // Sign-extended, then cut to the width below.
        AttributeValue::Sdata(value) => i128::from(*value) as u128,
        AttributeValue::Block(block) => {
            let bytes = block.to_slice().ok()?;
            if bytes.len() > 16 {
                return None;
            }
            let fold = |raw: u128, &byte: &u8| (raw << 8) | u128::from(byte);
            if endian.is_big_endian() {
                bytes.iter().fold(0, fold)
            } else {
                bytes.iter().rev().fold(0, fold)
            }
        }
        _ => return None,
    };
    Some(cut(raw, size))
}

fn cut(raw: u128, size: Option<u64>) -> u128
{
    match size {
        Some(bytes @ 1..=15) => raw & ((1_u128 << (bytes * 8)) - 1),
        _ => raw,
    }
}

/// The values and ranges of a `DW_AT_discr_list` block: `DW_DSC_label` value, or
/// `DW_DSC_range` low high, each LEB128 (signed when the discriminant is).
fn discriminant_list(list: &AttributeValue<OwnedReader>, size: Option<u64>, signed: bool) -> Option<Vec<DiscriminantRange>>
{
    let AttributeValue::Block(block) = list else {
        return None;
    };
    let mut reader = block.clone();
    let read = |reader: &mut OwnedReader| -> gimli::Result<u128> {
        Ok(if signed {
            cut(i128::from(reader.read_sleb128()?) as u128, size)
        } else {
            u128::from(reader.read_uleb128()?)
        })
    };
    let mut ranges = Vec::new();
    while !reader.is_empty() {
        let kind = reader.read_u8().ok()?;
        let low = read(&mut reader).ok()?;
        let high = if kind == constants::DW_DSC_range.0 {
            read(&mut reader).ok()?
        } else {
            low
        };
        ranges.push(DiscriminantRange { low, high });
    }
    Some(ranges)
}

fn base_encoding(entry: &Entry<'_, '_>) -> gimli::Result<BaseEncoding>
{
    Ok(match entry.attr_value(constants::DW_AT_encoding)? {
        Some(AttributeValue::Encoding(encoding)) => match encoding {
            constants::DW_ATE_signed => BaseEncoding::Signed,
            constants::DW_ATE_unsigned => BaseEncoding::Unsigned,
            constants::DW_ATE_float => BaseEncoding::Float,
            constants::DW_ATE_boolean => BaseEncoding::Boolean,
            constants::DW_ATE_UTF => BaseEncoding::Char,
            constants::DW_ATE_signed_char => BaseEncoding::SignedChar,
            constants::DW_ATE_unsigned_char => BaseEncoding::UnsignedChar,
            other => BaseEncoding::Other(other.0),
        },
        _ => BaseEncoding::Other(0),
    })
}

/// What a pointer entry is, from its tag and rustc's name for it.
fn pointer_kind(tag: DwTag, name: Option<&str>) -> PointerKind
{
    if matches!(
        tag,
        constants::DW_TAG_reference_type | constants::DW_TAG_rvalue_reference_type
    ) {
        return PointerKind::Reference { mutable: true };
    }
    let Some(name) = name else {
        return PointerKind::Raw { mutable: true };
    };
    let last = name.rsplit("::").next().unwrap_or(name);
    if name.starts_with("&mut ") {
        PointerKind::Reference { mutable: true }
    } else if name.starts_with('&') {
        PointerKind::Reference { mutable: false }
    } else if name.starts_with("*mut ") {
        PointerKind::Raw { mutable: true }
    } else if name.starts_with("*const ") {
        PointerKind::Raw { mutable: false }
    } else if name.starts_with("alloc::boxed::Box<") || last.starts_with("Box<") {
        PointerKind::Box
    } else {
        PointerKind::Other
    }
}

fn is_type_tag(tag: DwTag) -> bool
{
    matches!(
        tag,
        constants::DW_TAG_base_type
            | constants::DW_TAG_structure_type
            | constants::DW_TAG_class_type
            | constants::DW_TAG_union_type
            | constants::DW_TAG_enumeration_type
            | constants::DW_TAG_array_type
            | constants::DW_TAG_pointer_type
            | constants::DW_TAG_reference_type
            | constants::DW_TAG_rvalue_reference_type
            | constants::DW_TAG_typedef
            | constants::DW_TAG_const_type
            | constants::DW_TAG_volatile_type
            | constants::DW_TAG_restrict_type
            | constants::DW_TAG_atomic_type
            | constants::DW_TAG_immutable_type
            | constants::DW_TAG_subroutine_type
            | constants::DW_TAG_unspecified_type
            | constants::DW_TAG_ptr_to_member_type
    )
}

#[cfg(test)]
mod tests
{
    use std::collections::HashMap;

    use gimli::write::{self, AttributeValue as W, EndianVec, LineProgram, Sections, UnitEntryId};
    use gimli::{DwAt, DwTag, LittleEndian, RunTimeEndian, constants};

    use super::*;
    use crate::symbols::lines::synthetic::{ENCODING, TestDwarf};

    /// A unit under construction.
    struct Builder
    {
        unit: write::Unit,
    }

    impl Builder
    {
        fn new() -> Self
        {
            Self {
                unit: write::Unit::new(ENCODING, LineProgram::none()),
            }
        }

        fn root(&self) -> UnitEntryId
        {
            self.unit.root()
        }

        fn add(&mut self, parent: UnitEntryId, tag: DwTag, attributes: &[(DwAt, W)]) -> UnitEntryId
        {
            let id = self.unit.add(parent, tag);
            for (name, value) in attributes {
                self.unit.get_mut(id).set(*name, value.clone());
            }
            id
        }

        fn set(&mut self, id: UnitEntryId, name: DwAt, value: W)
        {
            self.unit.get_mut(id).set(name, value);
        }

        fn table(self) -> TypeTable
        {
            let mut dwarf = write::Dwarf::new();
            dwarf.units.add(self.unit);
            let mut sections = Sections::new(EndianVec::new(LittleEndian));
            dwarf.write(&mut sections).expect("write DWARF");
            let mut bytes = HashMap::new();
            sections
                .for_each(|id, section| {
                    bytes.insert(id, section.slice().to_vec());
                    Ok::<_, gimli::Error>(())
                })
                .expect("collect sections");
            TypeTable::build(TestDwarf { sections: bytes }.load(), RunTimeEndian::Little)
        }
    }

    fn name(text: &str) -> (DwAt, W)
    {
        (constants::DW_AT_name, W::String(text.into()))
    }

    fn base(b: &mut Builder, text: &str, size: u8, encoding: gimli::DwAte) -> UnitEntryId
    {
        let root = b.root();
        b.add(
            root,
            constants::DW_TAG_base_type,
            &[
                name(text),
                (constants::DW_AT_byte_size, W::Data1(size)),
                (constants::DW_AT_encoding, W::Encoding(encoding)),
            ],
        )
    }

    fn member(b: &mut Builder, parent: UnitEntryId, text: Option<&str>, ty: UnitEntryId, offset: u8) -> UnitEntryId
    {
        let id = b.add(
            parent,
            constants::DW_TAG_member,
            &[
                (constants::DW_AT_type, W::UnitRef(ty)),
                (constants::DW_AT_data_member_location, W::Data1(offset)),
            ],
        );
        if let Some(text) = text {
            b.set(id, constants::DW_AT_name, W::String(text.into()));
        }
        id
    }

    fn kind(table: &TypeTable, id: TypeId) -> TypeKind
    {
        table.get(id).expect("resolved").kind.clone()
    }

    #[test]
    fn a_recursive_struct_resolves_to_itself_once()
    {
        let mut b = Builder::new();
        let u32_ty = base(&mut b, "u32", 4, constants::DW_ATE_unsigned);
        let root = b.root();
        let ns = b.add(root, constants::DW_TAG_namespace, &[name("list")]);
        let node = b.add(
            ns,
            constants::DW_TAG_structure_type,
            &[name("Node"), (constants::DW_AT_byte_size, W::Data1(16))],
        );
        let pointer = b.add(
            root,
            constants::DW_TAG_pointer_type,
            &[(constants::DW_AT_type, W::UnitRef(node))],
        );
        member(&mut b, node, Some("value"), u32_ty, 8);
        member(&mut b, node, Some("next"), pointer, 0);
        let table = b.table();

        let id = table.type_named("list::Node").expect("Node by qualified name");
        assert_eq!(table.type_named("::list::Node"), Some(id));
        assert_eq!(table.type_named("Node"), None, "names are qualified");
        let TypeKind::Struct(node) = kind(&table, id) else {
            panic!("Node is a struct");
        };
        let next = node.member("next").expect("next");
        assert_eq!(next.offset, Some(0));
        let TypeKind::Pointer(pointer) = kind(&table, next.ty.expect("typed")) else {
            panic!("next is a pointer");
        };
        assert_eq!(pointer.pointee, Some(id), "the pointee is the node itself");
        assert_eq!(pointer.kind, PointerKind::Raw { mutable: true });
        let pointer = table.get(next.ty.expect("typed")).expect("pointer");
        assert_eq!(pointer.name.as_deref(), Some("*mut list::Node"));
        assert_eq!(pointer.size, Some(8), "a pointer without a size has the address size");
        assert_eq!(table.len(), 3, "Node, its pointer and u32, each once");
        assert_eq!(table.type_named("list::Node"), Some(id));
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn variant_parts_keep_raw_discriminants_and_ranges()
    {
        let mut b = Builder::new();
        let i8_ty = base(&mut b, "i8", 1, constants::DW_ATE_signed);
        let root = b.root();
        let shape = b.add(
            root,
            constants::DW_TAG_structure_type,
            &[name("Shape"), (constants::DW_AT_byte_size, W::Data1(8))],
        );
        let part = b.add(shape, constants::DW_TAG_variant_part, &[]);
        let tag = member(&mut b, part, None, i8_ty, 0);
        b.set(tag, constants::DW_AT_artificial, W::Flag(true));
        b.set(part, constants::DW_AT_discr, W::UnitRef(tag));
        let variants: [(&str, Option<W>, Option<W>); 4] = [
            ("Minus", Some(W::Sdata(-1)), None),
            ("Three", Some(W::Data1(3)), None),
            // Label 5, then the range -3..=-2 (wrapping as raw bits: 0xfd..=0xfe).
            ("Listed", None, Some(W::Block(vec![0, 5, 1, 0x7d, 0x7e]))),
            ("Rest", None, None),
        ];
        for (text, value, list) in variants {
            let variant = b.add(part, constants::DW_TAG_variant, &[]);
            if let Some(value) = value {
                b.set(variant, constants::DW_AT_discr_value, value);
            }
            if let Some(list) = list {
                b.set(variant, constants::DW_AT_discr_list, list);
            }
            member(&mut b, variant, Some(text), i8_ty, 0);
        }
        let table = b.table();

        let TypeKind::Enum(shape) = kind(&table, table.type_named("Shape").expect("Shape")) else {
            panic!("a struct with a variant part is an enum");
        };
        let discriminant = shape.discriminant.as_ref().expect("discriminant");
        assert!(discriminant.artificial);
        assert_eq!(discriminant.offset, Some(0));
        let values = |text: &str| shape.variant(text).expect("variant").discriminant.clone();
        assert_eq!(values("Minus"), Discriminant::Values(vec![DiscriminantRange::single(0xff)]));
        assert_eq!(values("Three"), Discriminant::Values(vec![DiscriminantRange::single(3)]));
        assert_eq!(
            values("Listed"),
            Discriminant::Values(vec![
                DiscriminantRange::single(5),
                DiscriminantRange { low: 0xfd, high: 0xfe }
            ])
        );
        assert_eq!(values("Rest"), Discriminant::Default);
        let named = |raw| shape.variant_for(raw).and_then(|variant| variant.name.clone());
        assert_eq!(named(0xff).as_deref(), Some("Minus"));
        assert_eq!(named(0xfe).as_deref(), Some("Listed"));
        assert_eq!(named(7).as_deref(), Some("Rest"));
        assert!(DiscriminantRange { low: 0xfe, high: 1 }.contains(0));
        assert!(!DiscriminantRange { low: 0xfe, high: 1 }.contains(2));
    }

    #[test]
    fn arrays_typedefs_and_declarations_derive_names_and_sizes()
    {
        let mut b = Builder::new();
        let u16_ty = base(&mut b, "u16", 2, constants::DW_ATE_unsigned);
        let root = b.root();
        let array = b.add(
            root,
            constants::DW_TAG_array_type,
            &[(constants::DW_AT_type, W::UnitRef(u16_ty))],
        );
        b.add(
            array,
            constants::DW_TAG_subrange_type,
            &[(constants::DW_AT_count, W::Data1(4))],
        );
        b.add(
            array,
            constants::DW_TAG_subrange_type,
            &[(constants::DW_AT_upper_bound, W::Data1(2))],
        );
        let alias = b.add(
            root,
            constants::DW_TAG_typedef,
            &[name("Grid"), (constants::DW_AT_type, W::UnitRef(array))],
        );
        b.add(
            root,
            constants::DW_TAG_variable,
            &[name("grid"), (constants::DW_AT_type, W::UnitRef(alias))],
        );
        b.add(
            root,
            constants::DW_TAG_structure_type,
            &[name("Later"), (constants::DW_AT_declaration, W::Flag(true))],
        );
        let later = b.add(
            root,
            constants::DW_TAG_structure_type,
            &[name("Later"), (constants::DW_AT_byte_size, W::Data1(2))],
        );
        member(&mut b, later, Some("grid"), alias, 0);
        let table = b.table();

        let grid = table.type_named("Grid").expect("Grid");
        let indexed = &table.units[0];
        let mut entries = indexed.unit.entries();
        let variable = loop {
            let (_, entry) = entries.next_dfs().expect("read entry").expect("variable");
            if entry.tag() == constants::DW_TAG_variable {
                break DieOffset((indexed.start + entry.offset().0) as u64);
            }
        };
        assert_eq!(table.type_at(variable), None, "a variable is not a type");
        assert_eq!(table.type_of(variable), Some(grid), "follow the variable's DW_AT_type");
        let array = table.strip_aliases(grid);
        assert_ne!(array, grid);
        let array_ty = table.get(array).expect("array");
        assert_eq!(array_ty.name.as_deref(), Some("[[u16; 3]; 4]"));
        assert_eq!(table.type_named("[[u16; 3]; 4]"), Some(array));
        assert_eq!(table.type_named("[u16; 4]"), None, "lengths must match every dimension");
        assert_eq!(array_ty.size, Some(24));
        assert_eq!(
            array_ty.kind,
            TypeKind::Array(ArrayType {
                element: table.type_named("u16"),
                lengths: vec![Some(4), Some(3)],
            })
        );
        assert_eq!(
            table.get(grid).expect("typedef").size,
            Some(24),
            "a typedef has its target's size"
        );
        let later_ty = table.get(table.type_named("Later").expect("Later")).expect("Later");
        assert!(!later_ty.declaration, "the definition wins over a declaration listed first");
        assert_eq!(table.type_of(later_ty.die), None, "a struct has no DW_AT_type");
        assert_eq!(table.type_at(DieOffset(u64::MAX)), None);
    }

    #[test]
    fn overflowing_unit_refs_make_types_unreadable()
    {
        // A 32-bit DWARF v4 unit can still use an eight-byte unit-relative reference.
        let abbrev = vec![
            1,
            constants::DW_TAG_compile_unit.0 as u8,
            1,
            0,
            0,
            2,
            constants::DW_TAG_pointer_type.0 as u8,
            0,
            constants::DW_AT_name.0 as u8,
            constants::DW_FORM_string.0 as u8,
            constants::DW_AT_type.0 as u8,
            constants::DW_FORM_ref8.0 as u8,
            0,
            0,
            3,
            constants::DW_TAG_structure_type.0 as u8,
            1,
            constants::DW_AT_name.0 as u8,
            constants::DW_FORM_string.0 as u8,
            0,
            0,
            4,
            constants::DW_TAG_variant_part.0 as u8,
            0,
            constants::DW_AT_discr.0 as u8,
            constants::DW_FORM_ref8.0 as u8,
            0,
            0,
            0,
        ];
        let mut info = Vec::new();
        let mut add_unit = |entries: &[u8]| {
            info.extend_from_slice(&(7 + u32::try_from(entries.len()).expect("small unit")).to_le_bytes());
            info.extend_from_slice(&4_u16.to_le_bytes()); // Version.
            info.extend_from_slice(&0_u32.to_le_bytes()); // Abbreviation table offset.
            info.push(8); // Address size.
            info.extend_from_slice(entries);
        };
        add_unit(&[1, 0]);

        // Both references overflow when the reader adds the nonzero start of this unit.
        let mut entries = b"\x01\x02BadPointer\0".to_vec();
        entries.extend_from_slice(&u64::MAX.to_le_bytes());
        entries.extend_from_slice(b"\x03BadEnum\0\x04");
        entries.extend_from_slice(&u64::MAX.to_le_bytes());
        entries.extend_from_slice(&[0, 0]);
        add_unit(&entries);

        let table = TypeTable::build(
            TestDwarf {
                sections: HashMap::from([(gimli::SectionId::DebugAbbrev, abbrev), (gimli::SectionId::DebugInfo, info)]),
            }
            .load(),
            RunTimeEndian::Little,
        );
        for name in ["BadPointer", "BadEnum"] {
            let id = table.type_named(name).expect("indexed type");
            assert_eq!(
                kind(&table, id),
                TypeKind::Unreadable(gimli::Error::OffsetOutOfBounds.to_string()),
                "{name}"
            );
        }
    }

    #[test]
    fn a_union_member_may_omit_its_zero_offset()
    {
        let mut b = Builder::new();
        let u16_ty = base(&mut b, "u16", 2, constants::DW_ATE_unsigned);
        let union = b.add(
            b.root(),
            constants::DW_TAG_union_type,
            &[name("Word"), (constants::DW_AT_byte_size, W::Data1(2))],
        );
        b.add(
            union,
            constants::DW_TAG_member,
            &[name("bits"), (constants::DW_AT_type, W::UnitRef(u16_ty))],
        );
        let table = b.table();

        let word = table.type_named("Word").expect("Word");
        let TypeKind::Union(fields) = kind(&table, word) else {
            panic!("Word is a union");
        };
        let bits = fields.member("bits").expect("bits");
        assert_eq!(bits.ty, table.type_named("u16"));
        assert_eq!(bits.offset, Some(0));
    }
}
