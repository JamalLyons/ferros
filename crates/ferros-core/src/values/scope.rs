//! Find the parameters and local variables visible in one logical DWARF frame.

use std::collections::HashMap;

use gimli::{AttributeValue, DebugInfoOffset, DebuggingInformationEntry, DwAt, DwTag, Reader as _, UnitOffset, UnitRef};

use super::location::{LocationEvaluator, Unavailable, VariableValue, expression_at, read_constant};
use crate::error::Result;
use crate::symbols::frames::{DwarfFrames, die_error};
use crate::symbols::{ImageId, OwnedReader};
use crate::types::{Address, FrameKind, FrameRegisters};

/// A local variable or argument visible in a stopped logical stack frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable
{
    /// Source-level name.
    pub name: String,
    /// The type's DWARF name, when available.
    pub type_name: Option<String>,
    /// The image whose debug information contains these DIEs.
    pub image_id: ImageId,
    /// Section-relative `.debug_info` offset of the variable DIE. Uses the abstract
    /// DIE when there is no concrete instance.
    pub die_offset: DebugInfoOffset<usize>,
    /// Section-relative `.debug_info` offset of the type named by `DW_AT_type`.
    /// Absolute references to other units may require a scan of their headers.
    /// Type units referenced by signature (`DW_FORM_ref_sig8`) have no offset here.
    pub type_offset: Option<DebugInfoOffset<usize>>,
    /// The raw bytes and storage location, or why the variable could not be read.
    pub value: VariableValue,
}

type Unit<'a> = UnitRef<'a, OwnedReader>;
type Entry<'a, 'b> = DebuggingInformationEntry<'a, 'b, OwnedReader>;
type Attr = AttributeValue<OwnedReader>;
type DwarfResult<T> = gimli::Result<T>;

pub(crate) fn frame_variables(
    frames: DwarfFrames<'_>,
    pc: u64,
    kind: FrameKind,
    image_id: ImageId,
    registers: &FrameRegisters,
    read_memory: impl FnMut(Address, usize) -> Result<Vec<u8>>,
    relocate: impl Fn(u64) -> Option<Address>,
) -> std::result::Result<Vec<Variable>, String>
{
    let Some(selected) = frames.scope(kind) else {
        return Ok(Vec::new());
    };
    let unit = frames.unit;
    let owner_die = unit
        .entry(frames.owner)
        .map_err(|error| die_error(unit, frames.owner, error))?;
    let frame_base = owner_die
        .attr_value(gimli::DW_AT_frame_base)
        .map_err(Unavailable::from)
        .and_then(|attr| expression_at(unit, attr, pc));
    let mut evaluator = LocationEvaluator::new(unit, registers, read_memory, relocate, frame_base);
    let inspect = |entry: &Entry<'_, '_>, byte_size, constant| {
        let location = entry
            .attr_value(gimli::DW_AT_location)
            .map_err(Unavailable::from)
            .and_then(|attr| expression_at(unit, attr, pc));
        match location {
            Ok(Some(location)) => evaluator.read_value(location, byte_size),
            Ok(None) => read_constant(unit, constant, byte_size),
            Err(reason) => VariableValue::unavailable(reason),
        }
    };
    FrameScope { pc, image_id }.variables(unit, selected, inspect)
}

struct FrameScope
{
    pc: u64,
    image_id: ImageId,
}

type ScopedVariable = (DwTag, Variable);

impl FrameScope
{
    fn variables(
        &self,
        unit: Unit<'_>,
        selected: UnitOffset<usize>,
        mut inspect: impl FnMut(&Entry<'_, '_>, Option<u64>, DwarfResult<Option<Attr>>) -> VariableValue,
    ) -> std::result::Result<Vec<Variable>, String>
    {
        let mut variables = Vec::new();
        let mut pending = vec![(selected, None)];
        while let Some((offset, inherited_start)) = pending.pop() {
            let scope_error = |error| die_error(unit, offset, error);
            let mut tree = unit.entries_tree(Some(offset)).map_err(scope_error)?;
            let root = tree.root().map_err(scope_error)?;
            let start = match scope_start(unit, root.entry(), self.pc, inherited_start) {
                Ok(Some(start)) => Some(start),
                Ok(None) if offset == selected => None,
                Ok(None) => continue,
                Err(error) if offset == selected => return Err(scope_error(error)),
                Err(error) => {
                    warn_die(root.entry(), error);
                    continue;
                }
            };
            let origin = root.entry().attr_value(gimli::DW_AT_abstract_origin);
            let mut children = root.children();
            let mut nested = Vec::new();
            let mut concrete = Vec::new();
            let mut by_origin = HashMap::new();
            while let Some(child) = children.next().map_err(scope_error)? {
                let entry = child.entry();
                if entry.tag() == gimli::DW_TAG_lexical_block {
                    nested.push((entry.offset(), start));
                } else if is_variable(entry.tag()) {
                    // An out-of-scope concrete DIE still accounts for its abstract declaration.
                    match entry.attr_value(gimli::DW_AT_abstract_origin) {
                        Ok(Some(origin)) => {
                            if let Some(origin) = reference_offset(unit, origin) {
                                by_origin.insert(origin, concrete.len());
                            }
                        }
                        Ok(None) => {}
                        Err(error) => warn_die(entry, error),
                    }
                    concrete.push(self.variable(unit, entry, start, |size, constant| inspect(entry, size, constant)));
                }
            }
            match origin.and_then(|origin| {
                with_reference(unit, origin, |unit, offset| {
                    self.abstract_variables(unit, offset, start, &by_origin)
                })
            }) {
                Ok(Some(declarations)) => {
                    for declaration in declarations {
                        variables.extend(match declaration {
                            AbstractVariable::Concrete(index) => concrete[index].take(),
                            AbstractVariable::Omitted(variable) => Some(variable),
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(?offset, %error, "skipping abstract variable declarations"),
            }
            variables.extend(concrete.into_iter().flatten());
            pending.extend(nested.into_iter().rev());
        }
        // Stable order within each group, with outer scopes before their nested scopes.
        variables.sort_by_key(|(tag, _)| *tag != gimli::DW_TAG_formal_parameter);
        Ok(variables.into_iter().map(|(_, variable)| variable).collect())
    }

    fn abstract_variables(
        &self,
        unit: Unit<'_>,
        parent: UnitOffset<usize>,
        start: Option<u64>,
        by_origin: &HashMap<DebugInfoOffset<usize>, usize>,
    ) -> DwarfResult<Vec<AbstractVariable>>
    {
        let mut tree = unit.entries_tree(Some(parent))?;
        let mut children = tree.root()?.children();
        let mut declarations = Vec::new();
        // An abstract lexical block needs a concrete scope to locate it at this PC.
        while let Some(child) = children.next()? {
            let entry = child.entry();
            if !is_variable(entry.tag()) {
                continue;
            }
            let offset = entry.offset().to_debug_info_offset(&unit.header);
            if let Some(index) = offset.and_then(|offset| by_origin.get(&offset)) {
                declarations.push(AbstractVariable::Concrete(*index));
            } else if let Some(variable) =
                self.variable(unit, entry, start, |size, constant| read_constant(unit, constant, size))
            {
                declarations.push(AbstractVariable::Omitted(variable));
            }
        }
        Ok(declarations)
    }

    fn variable(
        &self,
        unit: Unit<'_>,
        entry: &Entry<'_, '_>,
        start: Option<u64>,
        inspect: impl FnOnce(Option<u64>, DwarfResult<Option<Attr>>) -> VariableValue,
    ) -> Option<ScopedVariable>
    {
        let name = match self.variable_name_in_scope(unit, entry, start) {
            Ok(name) => name?,
            Err(error) => {
                warn_die(entry, error);
                return None;
            }
        };
        let die_offset = entry.offset().to_debug_info_offset(&unit.header)?;
        let ty = inherited_attribute(
            unit,
            entry,
            gimli::DW_AT_type,
            &|unit, attr| with_reference(unit, Some(attr), |unit, offset| describe_type(unit, offset, 0)),
            0,
        );
        let (ty, value) = match ty {
            Ok(ty) => {
                let ty = ty.flatten().unwrap_or_default();
                let constant = inherited_attribute(unit, entry, gimli::DW_AT_const_value, &|_, attr| Ok(attr), 0);
                let value = inspect(ty.byte_size, constant);
                (ty, value)
            }
            Err(error) => (
                TypeDescription::default(),
                VariableValue::unavailable(Unavailable::InvalidMetadata(error)),
            ),
        };
        Some((
            entry.tag(),
            Variable {
                name,
                image_id: self.image_id,
                die_offset,
                type_name: ty.name,
                type_offset: ty.offset,
                value,
            },
        ))
    }

    fn variable_name_in_scope(
        &self,
        unit: Unit<'_>,
        entry: &Entry<'_, '_>,
        start: Option<u64>,
    ) -> DwarfResult<Option<String>>
    {
        if matches!(entry.attr_value(gimli::DW_AT_declaration)?, Some(Attr::Flag(true)))
            || !variable_in_scope(unit, entry, self.pc, start)?
        {
            return Ok(None);
        }
        inherited_attribute(unit, entry, gimli::DW_AT_name, &read_name, 0)
    }
}

enum AbstractVariable
{
    Concrete(usize),
    Omitted(ScopedVariable),
}

fn is_variable(tag: DwTag) -> bool
{
    matches!(tag, gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter)
}

fn reference_offset(unit: Unit<'_>, reference: Attr) -> Option<DebugInfoOffset<usize>>
{
    match reference {
        Attr::UnitRef(offset) => offset.to_debug_info_offset(&unit.header),
        Attr::DebugInfoRef(offset) => Some(offset),
        _ => None,
    }
}

fn warn_die(entry: &Entry<'_, '_>, error: gimli::Error)
{
    tracing::warn!(offset = ?entry.offset(), tag = %entry.tag(), %error, "skipping variable or scope DIE");
}

/// First range start for `DW_AT_start_scope`. Blocks without explicit ranges inherit
/// the surrounding scope. Blocks with explicit ranges must contain `pc`.
fn scope_start(unit: Unit<'_>, entry: &Entry<'_, '_>, pc: u64, inherited: Option<u64>) -> DwarfResult<Option<u64>>
{
    let mut ranges = unit.die_ranges(entry)?;
    let Some(first) = ranges.next()? else {
        return if entry.attr(gimli::DW_AT_ranges)?.is_some() || entry.attr(gimli::DW_AT_low_pc)?.is_some() {
            Ok(None)
        } else {
            Ok(inherited)
        };
    };
    let mut range = first;
    loop {
        if range.begin <= pc && pc < range.end {
            return Ok(Some(first.begin));
        }
        let Some(next) = ranges.next()? else { return Ok(None) };
        range = next;
    }
}

fn variable_in_scope(unit: Unit<'_>, entry: &Entry<'_, '_>, pc: u64, scope_start: Option<u64>) -> DwarfResult<bool>
{
    let contains_pc = |unit: Unit<'_>, attr: Attr| {
        if let Some(mut ranges) = unit.attr_ranges(attr.clone())? {
            while let Some(range) = ranges.next()? {
                if range.begin <= pc && pc < range.end {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let offset = attr.udata_value().ok_or(gimli::Error::UnsupportedAttributeForm)?;
        Ok(scope_start
            .and_then(|start| start.checked_add(offset))
            .is_some_and(|start| start <= pc))
    };
    Ok(inherited_attribute(unit, entry, gimli::DW_AT_start_scope, &contains_pc, 0)?.unwrap_or(true))
}

#[derive(Default)]
struct TypeDescription
{
    offset: Option<DebugInfoOffset<usize>>,
    name: Option<String>,
    byte_size: Option<u64>,
}

// Bound chains of DW_AT_abstract_origin, DW_AT_specification and type wrappers.
const MAX_REFERENCE_DEPTH: usize = 32;

fn inherited_attribute<T>(
    unit: Unit<'_>,
    entry: &Entry<'_, '_>,
    attribute: DwAt,
    read: &impl Fn(Unit<'_>, Attr) -> DwarfResult<T>,
    depth: usize,
) -> DwarfResult<Option<T>>
{
    if let Some(value) = entry.attr_value(attribute)? {
        return read(unit, value).map(Some);
    }
    if depth == MAX_REFERENCE_DEPTH {
        return Ok(None);
    }
    let origin = match entry.attr_value(gimli::DW_AT_abstract_origin)? {
        Some(origin) => Some(origin),
        None => entry.attr_value(gimli::DW_AT_specification)?,
    };
    Ok(with_reference(unit, origin, |unit, offset| {
        inherited_attribute(unit, &unit.entry(offset)?, attribute, read, depth + 1)
    })?
    .flatten())
}

fn describe_type(unit: Unit<'_>, offset: UnitOffset<usize>, depth: usize) -> DwarfResult<TypeDescription>
{
    let entry = unit.entry(offset)?;
    let mut name = entry
        .attr_value(gimli::DW_AT_name)?
        .map(|attr| read_name(unit, attr))
        .transpose()?;
    let mut byte_size = entry
        .attr_value(gimli::DW_AT_byte_size)?
        .and_then(|value| value.udata_value());
    let pointer = match entry.tag() {
        gimli::DW_TAG_pointer_type => Some("*"),
        gimli::DW_TAG_reference_type => Some("&"),
        gimli::DW_TAG_rvalue_reference_type => Some("&&"),
        _ => None,
    };
    let qualifier = match entry.tag() {
        gimli::DW_TAG_const_type => Some("const "),
        gimli::DW_TAG_volatile_type => Some("volatile "),
        gimli::DW_TAG_restrict_type => Some("restrict "),
        gimli::DW_TAG_atomic_type => Some("atomic "),
        _ => None,
    };
    if pointer.is_some() {
        byte_size = byte_size.or(Some(u64::from(unit.encoding().address_size)));
    }
    if depth < MAX_REFERENCE_DEPTH && (pointer.is_some() || qualifier.is_some() || entry.tag() == gimli::DW_TAG_typedef) {
        let target = entry.attr_value(gimli::DW_AT_type)?;
        if let Some(inner) = with_reference(unit, target.clone(), |unit, offset| describe_type(unit, offset, depth + 1))? {
            if pointer.is_none() {
                byte_size = byte_size.or(inner.byte_size);
            }
            name = name.or_else(|| inner.name.map(|inner| wrapped_name(&inner, pointer, qualifier)));
        } else if target.is_none() && pointer.is_some() && name.is_none() {
            name = Some(wrapped_name("void", pointer, qualifier));
        }
    }
    if byte_size.is_none() && entry.tag() == gimli::DW_TAG_array_type && depth < MAX_REFERENCE_DEPTH {
        byte_size = array_size(unit, &entry, depth)?;
    }
    Ok(TypeDescription {
        offset: offset.to_debug_info_offset(&unit.header),
        name,
        byte_size,
    })
}

fn wrapped_name(name: &str, pointer: Option<&str>, qualifier: Option<&str>) -> String
{
    format!("{}{name}{}", qualifier.unwrap_or(""), pointer.unwrap_or(""))
}

fn array_size(unit: Unit<'_>, entry: &Entry<'_, '_>, depth: usize) -> DwarfResult<Option<u64>>
{
    let element = with_reference(unit, entry.attr_value(gimli::DW_AT_type)?, |unit, offset| {
        describe_type(unit, offset, depth + 1)
    })?
    .and_then(|element| element.byte_size);
    let size = match entry.attr_value(gimli::DW_AT_byte_stride)? {
        Some(stride) => stride.udata_value(),
        None => match entry.attr_value(gimli::DW_AT_bit_stride)? {
            Some(stride) => stride
                .udata_value()
                .filter(|bits| bits.is_multiple_of(8))
                .map(|bits| bits / 8),
            None => element,
        },
    };
    let Some(mut size) = size else {
        return Ok(None);
    };
    let mut tree = unit.entries_tree(Some(entry.offset()))?;
    let mut children = tree.root()?.children();
    let mut has_dimension = false;
    while let Some(child) = children.next()? {
        let subrange = child.entry();
        if subrange.tag() != gimli::DW_TAG_subrange_type {
            continue;
        }
        let Some(count) = subrange_count(unit, subrange)? else {
            return Ok(None);
        };
        let Some(total) = size.checked_mul(count) else {
            return Ok(None);
        };
        size = total;
        has_dimension = true;
    }
    Ok(has_dimension.then_some(size))
}

fn subrange_count(unit: Unit<'_>, entry: &Entry<'_, '_>) -> DwarfResult<Option<u64>>
{
    if let Some(count) = entry.attr_value(gimli::DW_AT_count)? {
        return Ok(count.udata_value());
    }
    let index_type = entry.attr_value(gimli::DW_AT_type)?;
    let untyped = index_type.is_none();
    let signed = with_reference(unit, index_type, |unit, offset| signed_index_type(unit, offset, 0))?.unwrap_or(false);
    let lower = match entry.attr_value(gimli::DW_AT_lower_bound)? {
        Some(lower) => integer_bound(lower, signed),
        None => {
            let mut entries = unit.entries();
            let Some((_, root)) = entries.next_dfs()? else {
                return Ok(None);
            };
            match root.attr_value(gimli::DW_AT_language)? {
                Some(Attr::Language(language)) => language.default_lower_bound().map(|bound| bound as i128),
                _ => None,
            }
        }
    };
    let upper = match entry.attr_value(gimli::DW_AT_upper_bound)? {
        // Untyped fixed-width bounds are unsigned. Preserve LLVM's -1 sentinel
        // for a zero-based range only when a 32- or 64-bit all-ones upper bound
        // would otherwise imply at least 2^32 elements.
        Some(Attr::Data4(u32::MAX) | Attr::Data8(u64::MAX)) if untyped && lower == Some(0) => Some(-1),
        other => other.and_then(|upper| integer_bound(upper, signed)),
    };
    Ok(upper
        .zip(lower)
        .and_then(|(upper, lower)| u64::try_from(upper - lower + 1).ok()))
}

fn signed_index_type(unit: Unit<'_>, offset: UnitOffset<usize>, depth: usize) -> DwarfResult<bool>
{
    let ty = unit.entry(offset)?;
    if ty.tag() == gimli::DW_TAG_base_type {
        return Ok(matches!(
            ty.attr_value(gimli::DW_AT_encoding)?,
            Some(Attr::Encoding(
                gimli::DW_ATE_signed | gimli::DW_ATE_signed_char | gimli::DW_ATE_signed_fixed
            ))
        ));
    }
    if depth == MAX_REFERENCE_DEPTH {
        return Ok(false);
    }
    Ok(with_reference(unit, ty.attr_value(gimli::DW_AT_type)?, |unit, offset| {
        signed_index_type(unit, offset, depth + 1)
    })?
    .unwrap_or(false))
}

fn integer_bound(attr: Attr, signed: bool) -> Option<i128>
{
    match attr {
        Attr::Sdata(value) => Some(i128::from(value)),
        Attr::Udata(value) => Some(i128::from(value)),
        other if signed => other.sdata_value().map(i128::from),
        other => other.udata_value().map(i128::from),
    }
}

fn read_name(unit: Unit<'_>, attr: Attr) -> DwarfResult<String>
{
    Ok(unit.attr_string(attr)?.to_string_lossy()?.into_owned())
}

fn with_reference<T>(
    unit: Unit<'_>,
    reference: Option<Attr>,
    read: impl FnOnce(Unit<'_>, UnitOffset<usize>) -> DwarfResult<T>,
) -> DwarfResult<Option<T>>
{
    match reference {
        Some(Attr::UnitRef(offset)) => read(unit, offset).map(Some),
        Some(Attr::DebugInfoRef(absolute)) => {
            if let Some(offset) = absolute.to_unit_offset(&unit.header) {
                return read(unit, offset).map(Some);
            }
            let mut units = unit.dwarf.units();
            while let Some(header) = units.next()? {
                if let Some(offset) = absolute.to_unit_offset(&header) {
                    let target = unit.dwarf.unit(header)?;
                    return read(UnitRef::new(unit.dwarf, &target), offset).map(Some);
                }
            }
            Err(gimli::Error::NoEntryAtGivenOffset)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests
{
    use std::path::Path;

    use gimli::RunTimeEndian;
    use gimli::write::{Address as FileAddress, AttributeValue as WriteAttr, DwarfUnit, Expression, Location, LocationList};

    use super::*;
    use crate::symbols::OwnedDwarf;
    use crate::symbols::frames::FrameIndex;
    use crate::values::location::ValueLocation;
    use crate::values::location::tests::{encoding, synthetic_dwarf};

    fn test_function(unit: &mut gimli::write::Unit, name: Option<&str>) -> gimli::write::UnitEntryId
    {
        let root = unit.root();
        let addr = WriteAttr::Address(FileAddress::Constant(0x100));
        unit.get_mut(root).set(gimli::DW_AT_low_pc, addr.clone());
        unit.get_mut(root).set(gimli::DW_AT_high_pc, WriteAttr::Udata(0x30));
        let function = unit.add(root, gimli::DW_TAG_subprogram);
        let entry = unit.get_mut(function);
        if let Some(name) = name {
            entry.set(gimli::DW_AT_name, WriteAttr::String(name.as_bytes().to_vec()));
        }
        entry.set(gimli::DW_AT_low_pc, addr);
        entry.set(gimli::DW_AT_high_pc, WriteAttr::Udata(0x30));
        function
    }

    fn test_frame(dwarf: &OwnedDwarf, index: &FrameIndex, pc: u64) -> std::result::Result<Vec<Variable>, String>
    {
        frame_variables(
            index.lookup(dwarf, pc)?.expect("function at PC"),
            pc,
            FrameKind::Physical,
            ImageId::from_parts(Path::new("test"), 0),
            &FrameRegisters::new(None, |reg| (reg == 0).then_some(7)),
            |_, _| panic!("test variables should not read memory"),
            |_| None,
        )
    }

    #[test]
    fn skips_a_bad_variable_name_between_valid_dies()
    {
        let mut dwarf = DwarfUnit::new(encoding(4));
        let function = test_function(&mut dwarf.unit, Some("function"));
        let bad_name = dwarf.strings.add(b"bad_name".to_vec());
        for name in [
            WriteAttr::String(b"first".to_vec()),
            WriteAttr::StringRef(bad_name),
            WriteAttr::String(b"last".to_vec()),
        ] {
            let variable = dwarf.unit.add(function, gimli::DW_TAG_variable);
            dwarf.unit.get_mut(variable).set(gimli::DW_AT_name, name);
        }
        let mut dwarf = synthetic_dwarf(dwarf, RunTimeEndian::Little);
        // Only the middle variable uses this section. Its offset now points past the end.
        dwarf.debug_str = gimli::DebugStr::from(OwnedReader::new(std::sync::Arc::from([]), RunTimeEndian::Little));
        let vars = test_frame(&dwarf, &FrameIndex::build(&dwarf).unwrap(), 0x105).unwrap();
        assert_eq!(
            vars.iter().map(|var| var.name.as_str()).collect::<Vec<_>>(),
            ["first", "last"]
        );
    }

    #[test]
    fn named_inline_uses_its_abstract_variables_and_scope_under_an_unnamed_owner()
    {
        let mut dwarf = DwarfUnit::new(encoding(4));
        let unit = &mut dwarf.unit;
        let owner = test_function(unit, None);
        let abstract_fn = unit.add(unit.root(), gimli::DW_TAG_subprogram);
        unit.get_mut(abstract_fn)
            .set(gimli::DW_AT_name, WriteAttr::String(b"inlined".to_vec()));
        let ty = unit.add(unit.root(), gimli::DW_TAG_base_type);
        unit.get_mut(ty).set(gimli::DW_AT_byte_size, WriteAttr::Udata(1));
        let omitted = unit.add(abstract_fn, gimli::DW_TAG_formal_parameter);
        unit.get_mut(omitted)
            .set(gimli::DW_AT_name, WriteAttr::String(b"omitted".to_vec()));
        let abstract_local = unit.add(abstract_fn, gimli::DW_TAG_variable);
        let variable = unit.get_mut(abstract_local);
        variable.set(gimli::DW_AT_name, WriteAttr::String(b"started".to_vec()));
        variable.set(gimli::DW_AT_type, WriteAttr::UnitRef(ty));
        variable.set(gimli::DW_AT_start_scope, WriteAttr::Udata(0x8));

        let inline = unit.add(owner, gimli::DW_TAG_inlined_subroutine);
        let entry = unit.get_mut(inline);
        entry.set(gimli::DW_AT_abstract_origin, WriteAttr::UnitRef(abstract_fn));
        entry.set(gimli::DW_AT_low_pc, WriteAttr::Address(FileAddress::Constant(0x104)));
        entry.set(gimli::DW_AT_high_pc, WriteAttr::Udata(0x20));
        let concrete = unit.add(inline, gimli::DW_TAG_variable);
        let mut location = Expression::new();
        location.op_reg(gimli::Register(0));
        unit.get_mut(concrete)
            .set(gimli::DW_AT_abstract_origin, WriteAttr::UnitRef(abstract_local));
        unit.get_mut(concrete)
            .set(gimli::DW_AT_location, WriteAttr::Exprloc(location));

        let dwarf = synthetic_dwarf(dwarf, RunTimeEndian::Little);
        let index = FrameIndex::build(&dwarf).unwrap();
        let vars = test_frame(&dwarf, &index, 0x10b).unwrap();
        assert_eq!(vars.len(), 1, "before inherited start scope");
        assert_eq!(vars[0].name, "omitted");
        assert_eq!(vars[0].value.bytes, Err(Unavailable::OptimizedOut));
        let vars = test_frame(&dwarf, &index, 0x10c).unwrap();
        assert_eq!(
            vars.iter().map(|var| var.name.as_str()).collect::<Vec<_>>(),
            ["omitted", "started"]
        );
        assert_eq!(vars[1].value.bytes.as_deref(), Ok(&[7][..]));
    }

    #[test]
    fn selects_pc_ranges_in_legacy_and_dwarf5_location_lists()
    {
        for version in [4, 5] {
            let mut dwarf = DwarfUnit::new(encoding(version));
            let unit = &mut dwarf.unit;
            let root = unit.root();
            let addr = |value| WriteAttr::Address(FileAddress::Constant(value));
            unit.get_mut(root).set(gimli::DW_AT_low_pc, addr(0x100));
            unit.get_mut(root).set(gimli::DW_AT_high_pc, WriteAttr::Udata(0x30));

            let int = unit.add(root, gimli::DW_TAG_base_type);
            unit.get_mut(int).set(gimli::DW_AT_name, WriteAttr::String(b"u16".to_vec()));
            unit.get_mut(int).set(gimli::DW_AT_byte_size, WriteAttr::Udata(2));

            let function = unit.add(root, gimli::DW_TAG_subprogram);
            let subprogram = unit.get_mut(function);
            subprogram.set(gimli::DW_AT_name, WriteAttr::String(b"function".to_vec()));
            subprogram.set(gimli::DW_AT_low_pc, addr(0x100));
            subprogram.set(gimli::DW_AT_high_pc, WriteAttr::Udata(0x30));

            let mut base = Expression::new();
            base.op(gimli::DW_OP_call_frame_cfa);
            let base_list = unit.locations.add(LocationList(vec![Location::OffsetPair {
                begin: 0x00,
                end: 0x20,
                data: base,
            }]));
            unit.get_mut(function)
                .set(gimli::DW_AT_frame_base, WriteAttr::LocationListRef(base_list));

            let mut reg = Expression::new();
            reg.op_reg(gimli::Register(0));
            let mut stack = Expression::new();
            stack.op_fbreg(-8);
            let mut entries = vec![
                Location::BaseAddress {
                    address: FileAddress::Constant(0x100),
                },
                Location::OffsetPair {
                    begin: 0,
                    end: 8,
                    data: reg.clone(),
                },
                Location::OffsetPair {
                    begin: 8,
                    end: 0x10,
                    data: stack.clone(),
                },
                Location::OffsetPair {
                    begin: 0x20,
                    end: 0x28,
                    data: Expression::new(),
                },
            ];
            if version == 5 {
                let mut default = Expression::new();
                default.op(gimli::DW_OP_lit7);
                default.op(gimli::DW_OP_stack_value);
                entries.insert(0, Location::DefaultLocation { data: default });
            }
            let list = unit.locations.add(LocationList(entries));
            let moving = unit.add(function, gimli::DW_TAG_formal_parameter);
            let moving = unit.get_mut(moving);
            moving.set(gimli::DW_AT_name, WriteAttr::String(b"moving".to_vec()));
            moving.set(gimli::DW_AT_type, WriteAttr::UnitRef(int));
            moving.set(gimli::DW_AT_location, WriteAttr::LocationListRef(list));

            let late = unit.add(function, gimli::DW_TAG_variable);
            let late = unit.get_mut(late);
            late.set(gimli::DW_AT_name, WriteAttr::String(b"late".to_vec()));
            late.set(gimli::DW_AT_type, WriteAttr::UnitRef(int));
            late.set(gimli::DW_AT_start_scope, WriteAttr::Udata(0x20));
            late.set(gimli::DW_AT_location, WriteAttr::Exprloc(stack));

            let dwarf = synthetic_dwarf(dwarf, RunTimeEndian::Little);
            let index = FrameIndex::build(&dwarf).unwrap();
            let image = ImageId::from_parts(Path::new("test"), 0);
            let read_frame = |pc, known_reg| {
                let registers = FrameRegisters::new(Some(Address::new(0x8010)), |n| (n == 0 && known_reg).then_some(0x1234));
                frame_variables(
                    index.lookup(&dwarf, pc).unwrap().expect("function at PC"),
                    pc,
                    FrameKind::Physical,
                    image,
                    &registers,
                    |address, len| {
                        assert_eq!((address, len), (Address::new(0x8008), 2));
                        Ok(vec![0xcd, 0xab])
                    },
                    |_| None,
                )
                .unwrap()
            };
            let first = read_frame(0x107, true);
            assert_eq!(first.len(), 1);
            assert_eq!(first[0].name, "moving");
            assert_eq!(first[0].type_name.as_deref(), Some("u16"));
            assert_eq!(first[0].value.location, Some(ValueLocation::Register(0)));
            assert_eq!(first[0].value.bytes.as_deref(), Ok(&[0x34, 0x12][..]));
            assert_eq!(read_frame(0x107, false)[0].value.bytes, Err(Unavailable::RegisterUnknown(0)));

            let next = read_frame(0x108, false);
            assert_eq!(next[0].value.location, Some(ValueLocation::Memory(Address::new(0x8008))));
            assert_eq!(next[0].value.bytes.as_deref(), Ok(&[0xcd, 0xab][..]));
            let gap = read_frame(0x110, false);
            if version == 5 {
                assert_eq!(gap[0].value.location, Some(ValueLocation::Implicit));
                assert_eq!(gap[0].value.bytes.as_deref(), Ok(&[7, 0][..]));
            } else {
                assert_eq!(gap[0].value.bytes, Err(Unavailable::OptimizedOut));
            }

            let last = read_frame(0x120, false);
            assert_eq!(last.len(), 2);
            let moving = last.iter().find(|var| var.name == "moving").unwrap();
            assert_eq!(moving.value.bytes, Err(Unavailable::OptimizedOut));
            let late = last.iter().find(|var| var.name == "late").unwrap();
            assert_eq!(late.value.bytes, Err(Unavailable::FrameBaseUnknown));
        }
    }
}
