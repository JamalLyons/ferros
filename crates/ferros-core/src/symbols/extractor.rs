//! DWARF type extraction and introspection.

use gimli::{
    constants, Attribute, AttributeValue, DebugTypeSignature, DebuggingInformationEntry, Reader, Unit, UnitOffset,
    UnitSectionOffset, UnitType,
};

use super::demangle::{is_trait_object, map_dwarf_error};
use super::{OwnedDwarf, OwnedReader};
use crate::error::Result;

const MAX_TYPE_REF_DEPTH: usize = 32;

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

pub(crate) struct TypeExtractor<'a>
{
    dwarf: &'a OwnedDwarf,
    units: Vec<Unit<OwnedReader>>,
}

impl<'a> TypeExtractor<'a>
{
    pub(crate) fn new(dwarf: &'a OwnedDwarf) -> Result<Self>
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

    pub(crate) fn describe(&self, target: &str) -> Result<Option<TypeSummary>>
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
