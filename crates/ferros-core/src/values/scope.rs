//! Find the parameters and local variables visible in one logical DWARF frame.

use addr2line::Context;
use gimli::{AttributeValue, DebugInfoOffset, DebuggingInformationEntry, Reader as _, UnitOffset, UnitRef};

use super::location::{LocationEvaluator, Unavailable, VariableValue, expression_at};
use crate::error::Result;
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
    /// Section-relative `.debug_info` offset of the concrete variable DIE.
    pub die_offset: DebugInfoOffset<usize>,
    /// Section-relative `.debug_info` offset of the type named by `DW_AT_type`.
    pub type_offset: Option<DebugInfoOffset<usize>>,
    /// The raw bytes and storage location, or why the variable could not be read.
    pub value: VariableValue,
}

type Unit<'a> = UnitRef<'a, OwnedReader>;
type Entry<'a, 'b> = DebuggingInformationEntry<'a, 'b, OwnedReader>;
type Attr = AttributeValue<OwnedReader>;
type DwarfResult<T> = gimli::Result<T>;

pub(crate) fn frame_variables(
    context: &Context<OwnedReader>,
    pc: u64,
    kind: FrameKind,
    image_id: ImageId,
    registers: &FrameRegisters,
    read_memory: impl FnMut(Address, usize) -> Result<Vec<u8>>,
    relocate: impl Fn(u64) -> Option<Address>,
) -> DwarfResult<Vec<Variable>>
{
    let Some(unit) = context.find_dwarf_and_unit(pc).skip_all_loads() else {
        return Ok(Vec::new());
    };
    let mut frames = context.find_frames(pc).skip_all_loads()?;
    let mut owner = None;
    let mut inline = None;
    let mut named_depth = 0;
    while let Some(frame) = frames.next()? {
        owner = frame.dw_die_offset;
        // Stack symbolication includes only frames with a readable name.
        if frame.function.is_some_and(|name| name.raw_name().is_ok()) {
            if matches!(kind, FrameKind::Inlined { depth, .. } if usize::from(depth) == named_depth) {
                inline = frame.dw_die_offset;
            }
            named_depth += 1;
        }
    }
    let Some(owner) = owner else { return Ok(Vec::new()) };
    let selected = match kind {
        FrameKind::Physical => owner,
        FrameKind::Inlined { .. } => match inline {
            Some(inline) => inline,
            None => return Ok(Vec::new()),
        },
    };
    let owner_die = unit.entry(owner)?;
    let frame_base = expression_at(unit, owner_die.attr_value(gimli::DW_AT_frame_base)?, pc);
    let mut evaluator = LocationEvaluator::new(unit, registers, read_memory, relocate, frame_base);
    let mut variables = Vec::new();
    let mut entries = unit.entries_at_offset(selected)?;
    let Some((_, scope)) = entries.next_dfs()? else {
        return Ok(variables);
    };
    let mut active_scopes = vec![scope_start(unit, scope, pc, None)?];
    let mut depth = 0;
    while let Some((delta, entry)) = entries.next_dfs()? {
        depth += delta;
        if depth <= 0 {
            break;
        }
        let depth = depth as usize;
        if depth > active_scopes.len() {
            continue;
        }
        active_scopes.truncate(depth);
        let parent_start = active_scopes[depth - 1];
        match entry.tag() {
            gimli::DW_TAG_lexical_block => {
                if let Some(start) = scope_start(unit, entry, pc, parent_start)? {
                    active_scopes.push(Some(start));
                }
            }
            gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter => {
                if matches!(entry.attr_value(gimli::DW_AT_declaration)?, Some(Attr::Flag(true)))
                    || !variable_in_scope(unit, entry, pc, parent_start)?
                {
                    continue;
                }
                let metadata = metadata(unit, entry, 0)?;
                let Some(name) = metadata.name else { continue };
                let Some(die_offset) = entry.offset().to_debug_info_offset(&unit.header) else {
                    continue;
                };
                let ty = metadata.ty.unwrap_or_default();
                let value = match expression_at(unit, entry.attr_value(gimli::DW_AT_location)?, pc) {
                    Ok(Some(location)) => evaluator.read_value(location, ty.byte_size),
                    Ok(None) => match metadata.constant {
                        Some(constant) => evaluator.read_constant(constant, ty.byte_size),
                        None => VariableValue::unavailable(Unavailable::OptimizedOut),
                    },
                    Err(reason) => VariableValue::unavailable(reason),
                };
                variables.push(Variable {
                    name,
                    type_name: ty.name,
                    image_id,
                    die_offset,
                    type_offset: ty.offset,
                    value,
                });
            }
            // Other inline instances are distinct logical frames.
            _ => {}
        }
    }
    // Deeper lexical bindings precede outer bindings with the same name. Retain all
    // visible DIEs so callers can also examine a shadowed binding.
    variables.reverse();
    Ok(variables)
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
    let Some(attr) = entry.attr_value(gimli::DW_AT_start_scope)? else {
        return Ok(true);
    };
    if let Some(mut ranges) = unit.attr_ranges(attr.clone())? {
        while let Some(range) = ranges.next()? {
            if range.begin <= pc && pc < range.end {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    Ok(scope_start
        .zip(attr.udata_value())
        .and_then(|(start, offset)| start.checked_add(offset))
        .is_some_and(|start| start <= pc))
}

#[derive(Default)]
struct Metadata
{
    name: Option<String>,
    ty: Option<TypeDescription>,
    constant: Option<Attr>,
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

fn metadata(unit: Unit<'_>, entry: &Entry<'_, '_>, depth: usize) -> DwarfResult<Metadata>
{
    let mut info = Metadata {
        name: name(unit, entry)?,
        ty: with_reference(unit, entry.attr_value(gimli::DW_AT_type)?, |unit, offset| {
            describe_type(unit, offset, 0)
        })?,
        constant: entry.attr_value(gimli::DW_AT_const_value)?,
    };
    if depth < MAX_REFERENCE_DEPTH && (info.name.is_none() || info.ty.is_none() || info.constant.is_none()) {
        let origin = match entry.attr_value(gimli::DW_AT_abstract_origin)? {
            Some(origin) => Some(origin),
            None => entry.attr_value(gimli::DW_AT_specification)?,
        };
        if let Some(origin) = with_reference(unit, origin, |unit, offset| metadata(unit, &unit.entry(offset)?, depth + 1))? {
            info.name = info.name.or(origin.name);
            info.ty = info.ty.or(origin.ty);
            info.constant = info.constant.or(origin.constant);
        }
    }
    Ok(info)
}

fn describe_type(unit: Unit<'_>, offset: UnitOffset<usize>, depth: usize) -> DwarfResult<TypeDescription>
{
    let entry = unit.entry(offset)?;
    let mut name = name(unit, &entry)?;
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

fn name(unit: Unit<'_>, entry: &Entry<'_, '_>) -> DwarfResult<Option<String>>
{
    let Some(value) = entry.attr_value(gimli::DW_AT_name)? else {
        return Ok(None);
    };
    Ok(Some(unit.attr_string(value)?.to_string_lossy()?.into_owned()))
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
            Ok(None)
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
    use crate::values::location::ValueLocation;
    use crate::values::location::tests::{encoding, synthetic_dwarf};

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

            let context = Context::from_dwarf(synthetic_dwarf(dwarf, RunTimeEndian::Little)).unwrap();
            let image = ImageId::from_parts(Path::new("test"), 0);
            let read_frame = |pc, known_reg| {
                let registers = FrameRegisters::new(Some(Address::new(0x8010)), |n| (n == 0 && known_reg).then_some(0x1234));
                frame_variables(
                    &context,
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
