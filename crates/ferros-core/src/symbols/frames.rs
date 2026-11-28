//! Resolve the compilation unit and its logical frames together. Function and inline
//! indexes are built only for units and subprograms reached by a lookup.

use std::collections::HashMap;

use gimli::{AttributeValue, DebugInfoOffset, DwTag, Range, Reader as _, UnitOffset, UnitType};
use once_cell::sync::OnceCell;

use super::cache::SymbolFrame;
use super::demangle::make_symbol_name;
use super::functions::unit_without_lines;
use super::lines::{LineSequence, read_sequences, render_file};
use super::{OwnedDwarf, OwnedReader};
use crate::types::{FrameKind, SourceLocation, SymbolSource};

type Unit<'a> = gimli::UnitRef<'a, OwnedReader>;
type Die<'a, 'b> = gimli::DebuggingInformationEntry<'a, 'b, OwnedReader>;

/// Store nested and overlapping ranges, ordered by end address. The suffix
/// minimum lets a lookup stop as soon as no later range can contain the PC.
struct AddressIndex<T>(Vec<(Range, T, u64)>);

impl<T> AddressIndex<T>
{
    fn new(entries: impl IntoIterator<Item = (Range, T)>) -> Self
    {
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter(|(range, _)| range.begin < range.end)
            .map(|(range, value)| (range, value, 0))
            .collect();
        entries.sort_by_key(|(range, _, _)| range.end);
        let mut min_begin = u64::MAX;
        for (range, _, suffix_min) in entries.iter_mut().rev() {
            min_begin = min_begin.min(range.begin);
            *suffix_min = min_begin;
        }
        Self(entries)
    }

    fn covering(&self, pc: u64) -> impl Iterator<Item = &T>
    {
        let first = self.0.partition_point(|(range, _, _)| range.end <= pc);
        self.0[first..]
            .iter()
            .take_while(move |(_, _, suffix_min)| *suffix_min <= pc)
            .filter_map(move |(range, value, _)| (range.begin <= pc).then_some(value))
    }
}

pub(crate) struct FrameIndex
{
    units: Vec<FrameUnit>,
    primary: AddressIndex<usize>,
    bounds: AddressIndex<usize>,
}

struct FrameUnit
{
    offset: DebugInfoOffset<usize>,
    unit: gimli::Unit<OwnedReader>,
    functions: OnceCell<Result<Functions, String>>,
    lines: OnceCell<FrameLines>,
}

impl FrameIndex
{
    pub(crate) fn build(dwarf: &OwnedDwarf) -> gimli::Result<Self>
    {
        let mut aranges = read_aranges(dwarf);
        let mut units = Vec::new();
        let mut primary = Vec::new();
        let mut fallback = Vec::new();
        let mut headers = dwarf.units();
        while let Some(header) = headers.next()? {
            let Some(offset) = header.offset().as_debug_info_offset() else {
                continue;
            };
            let has_code = match header.type_() {
                UnitType::Type { .. } | UnitType::SplitType { .. } => continue,
                UnitType::Partial => false,
                _ => true,
            };
            let unit = match dwarf.unit(header.clone()).or_else(|_| unit_without_lines(dwarf, header)) {
                Ok(unit) => unit,
                Err(error) => {
                    tracing::debug!(unit = offset.0, %error, "skipping unreadable frame unit");
                    continue;
                }
            };
            let index = units.len();
            let record = FrameUnit {
                offset,
                unit,
                functions: OnceCell::new(),
                lines: OnceCell::new(),
            };
            if has_code {
                let (ranges, bounds) = unit_ranges(Unit::new(dwarf, &record.unit)).unwrap_or_else(|error| {
                    tracing::debug!(unit = offset.0, %error, "ignoring unreadable unit ranges");
                    (Vec::new(), None)
                });
                if !ranges.is_empty() {
                    primary.extend(ranges.into_iter().map(|range| (range, index)));
                } else if let Some(ranges) = aranges.remove(&offset) {
                    primary.extend(ranges.into_iter().map(|range| (range, index)));
                    // A producer may list only some of the CU's functions in aranges.
                    fallback.extend(bounds.map(|range| (range, index)));
                } else if let Some(range) = bounds {
                    primary.push((range, index));
                } else {
                    primary.extend(record.lines(dwarf).sequences.0.iter().map(|(range, _, _)| (*range, index)));
                }
            }
            units.push(record);
        }
        Ok(Self {
            units,
            primary: AddressIndex::new(primary),
            bounds: AddressIndex::new(fallback),
        })
    }

    pub(crate) fn lookup<'a>(&'a self, dwarf: &'a OwnedDwarf, pc: u64) -> Result<Option<DwarfFrames<'a>>, String>
    {
        let mut first_error = None;
        for &index in self.primary.covering(pc).chain(self.bounds.covering(pc)) {
            let record = &self.units[index];
            let unit = Unit::new(dwarf, &record.unit);
            let functions = match record.functions.get_or_init(|| Functions::read(unit)) {
                Ok(functions) => functions,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            let Some(&function) = functions.addresses.covering(pc).next() else {
                continue;
            };
            let (owner, frames) = &functions.entries[function];
            match frames.get_or_init(|| self.read_function(unit, *owner)) {
                Ok(frames) => {
                    return Ok(Some(DwarfFrames {
                        unit,
                        owner: *owner,
                        frames: frames.covering(pc),
                        record,
                    }));
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(None), |error| Err(error.clone()))
    }

    fn read_function(&self, unit: Unit<'_>, owner: UnitOffset<usize>) -> Result<FunctionFrames, String>
    {
        let mut frames = Vec::new();
        let mut children: Vec<Vec<_>> = Vec::new();
        let mut parents = Vec::<(isize, usize)>::new();
        let mut nested_function = None;
        walk_dies(unit, Some(owner), |offset, depth, tag| {
            let at_die = |error| die_error(unit, offset, error);
            while parents.last().is_some_and(|&(parent_depth, _)| depth <= parent_depth) {
                parents.pop();
            }
            if let Some(nested_depth) = nested_function {
                if depth > nested_depth {
                    return Ok(());
                }
                nested_function = None;
            }
            if tag == gimli::DW_TAG_subprogram && offset != owner {
                nested_function = Some(depth);
                return Ok(());
            }
            if offset == owner || tag == gimli::DW_TAG_inlined_subroutine {
                let die = unit.entry(offset).map_err(at_die)?;
                let index = frames.len();
                let call_site = if let Some(&(_, parent)) = parents.last() {
                    children[parent].extend(preferred_ranges(unit, &die).map_err(at_die)?.map(|range| (range, index)));
                    CallSite::read(unit, &die).map_err(at_die)?
                } else {
                    None
                };
                frames.push(FunctionFrame {
                    offset,
                    name: self.function_name(unit, &die)?,
                    call_site,
                });
                children.push(Vec::new());
                parents.push((depth, index));
            }
            Ok(())
        })?;
        Ok(FunctionFrames {
            frames,
            children: children.into_iter().map(AddressIndex::new).collect(),
        })
    }

    fn function_name(&self, unit: Unit<'_>, entry: &Die<'_, '_>) -> Result<Option<String>, String>
    {
        let mut source_name = None;
        match self.linkage_name(unit, entry, &mut source_name, 16) {
            Ok(linkage) => Ok(linkage.or(source_name)),
            // A readable source name is still usable if inheritance is corrupt.
            Err(_) if source_name.is_some() => Ok(source_name),
            Err(error) => Err(error),
        }
    }

    fn linkage_name(
        &self,
        unit: Unit<'_>,
        entry: &Die<'_, '_>,
        source_name: &mut Option<String>,
        remaining: usize,
    ) -> Result<Option<String>, String>
    {
        if remaining == 0 {
            return Ok(None);
        }
        let at_die = |error| die_error(unit, entry.offset(), error);
        let read_name = |name| -> Result<Option<String>, String> {
            let Some(raw) = entry
                .attr_value(name)
                .map_err(at_die)?
                .and_then(|attr| unit.attr_string(attr).ok())
            else {
                return Ok(None);
            };
            Ok(Some(raw.to_string_lossy().map_err(at_die)?.into_owned()))
        };
        for name in [gimli::DW_AT_linkage_name, gimli::DW_AT_MIPS_linkage_name] {
            if let Some(linkage) = read_name(name)? {
                return Ok(Some(linkage));
            }
        }
        if source_name.is_none() {
            *source_name = read_name(gimli::DW_AT_name)?;
        }

        let mut first_error = None;
        for attribute in [gimli::DW_AT_abstract_origin, gimli::DW_AT_specification] {
            let reference = entry.attr_value(attribute).map_err(at_die)?;
            let Some((target, offset)) = reference.and_then(|reference| self.referenced_die(unit, reference)) else {
                continue;
            };
            let linkage = target
                .entry(offset)
                .map_err(|error| die_error(target, offset, error))
                .and_then(|origin| self.linkage_name(target, &origin, source_name, remaining - 1));
            match linkage {
                Ok(Some(linkage)) => return Ok(Some(linkage)),
                Ok(None) => (),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(None), Err)
    }

    fn referenced_die<'a>(
        &'a self,
        unit: Unit<'a>,
        reference: AttributeValue<OwnedReader>,
    ) -> Option<(Unit<'a>, UnitOffset<usize>)>
    {
        match reference {
            AttributeValue::UnitRef(offset) => Some((unit, offset)),
            AttributeValue::DebugInfoRef(offset) => {
                let end = self.units.partition_point(|record| record.offset <= offset);
                let record = &self.units[end.checked_sub(1)?];
                let offset = offset.to_unit_offset(&record.unit.header)?;
                Some((Unit::new(unit.dwarf, &record.unit), offset))
            }
            _ => None,
        }
    }
}

fn read_aranges(dwarf: &OwnedDwarf) -> HashMap<DebugInfoOffset<usize>, Vec<Range>>
{
    let mut by_unit = HashMap::<_, Vec<_>>::new();
    let mut headers = dwarf.debug_aranges.headers();
    while let Ok(Some(header)) = headers.next() {
        let mut entries = header.entries();
        while let Some(entry) = entries.next().transpose() {
            if let Ok(entry) = entry
                && entry.length() != 0
            {
                by_unit.entry(header.debug_info_offset()).or_default().push(entry.range());
            }
        }
    }
    by_unit
}

fn unit_ranges(unit: Unit<'_>) -> gimli::Result<(Vec<Range>, Option<Range>)>
{
    let mut entries = unit.entries();
    let (_, root) = entries.next_dfs()?.ok_or(gimli::Error::MissingUnitDie)?;
    die_ranges(unit, root)
}

fn preferred_ranges(unit: Unit<'_>, die: &Die<'_, '_>) -> gimli::Result<impl Iterator<Item = Range> + use<>>
{
    let (ranges, bounds) = die_ranges(unit, die)?;
    Ok(ranges.into_iter().chain(bounds))
}

/// Read a range list or bounds. A low PC may just supply the range-list base.
/// Checked addition skips tombstones with an overflowing size.
fn die_ranges(unit: Unit<'_>, die: &Die<'_, '_>) -> gimli::Result<(Vec<Range>, Option<Range>)>
{
    if let Some(attr) = die.attr_value(gimli::DW_AT_ranges)?
        && let Some(mut iter) = unit.attr_ranges(attr)?
    {
        let mut ranges = Vec::new();
        while let Some(range) = iter.next()? {
            if range.begin < range.end {
                ranges.push(range);
            }
        }
        return Ok((ranges, None));
    }
    let bounds = match (die.attr_value(gimli::DW_AT_low_pc)?, die.attr_value(gimli::DW_AT_high_pc)?) {
        (Some(low), Some(high)) => {
            if let Some(begin) = unit.attr_address(low)? {
                let end = match high {
                    AttributeValue::Udata(size) => begin.checked_add(size),
                    address => unit.attr_address(address)?,
                };
                end.filter(|&end| begin < end).map(|end| Range { begin, end })
            } else {
                None
            }
        }
        _ => None,
    };
    Ok((Vec::new(), bounds))
}

fn walk_dies(
    unit: Unit<'_>,
    start: Option<UnitOffset<usize>>,
    mut visit: impl FnMut(UnitOffset<usize>, isize, DwTag) -> Result<(), String>,
) -> Result<(), String>
{
    let mut entries = unit.entries_raw(start).map_err(|error| match start {
        Some(offset) => die_error(unit, offset, error),
        None => error.to_string(),
    })?;
    let root_depth = entries.next_depth();
    while !entries.is_empty() {
        let offset = entries.next_offset();
        let depth = entries.next_depth();
        if start.is_some() && start != Some(offset) && depth <= root_depth {
            break;
        }
        let at_die = |error| die_error(unit, offset, error);
        if let Some(abbrev) = entries.read_abbreviation().map_err(at_die)? {
            visit(offset, depth, abbrev.tag())?;
            entries.skip_attributes(abbrev.attributes()).map_err(at_die)?;
        }
    }
    Ok(())
}

pub(crate) fn die_error(unit: Unit<'_>, offset: UnitOffset<usize>, error: impl std::fmt::Display) -> String
{
    let base = unit.header.offset().as_debug_info_offset().map_or(0, |offset| offset.0);
    format!("unit at .debug_info+{base:#x}, DIE at unit offset {:#x}: {error}", offset.0)
}

type FunctionEntry = (UnitOffset<usize>, OnceCell<Result<FunctionFrames, String>>);

struct Functions
{
    entries: Vec<FunctionEntry>,
    addresses: AddressIndex<usize>,
}

impl Functions
{
    fn read(unit: Unit<'_>) -> Result<Self, String>
    {
        let mut entries = Vec::new();
        let mut addresses = Vec::new();
        walk_dies(unit, None, |offset, _, tag| {
            if tag == gimli::DW_TAG_subprogram {
                let at_die = |error| die_error(unit, offset, error);
                let entry = unit.entry(offset).map_err(at_die)?;
                let mut ranges = preferred_ranges(unit, &entry).map_err(at_die)?.peekable();
                if ranges.peek().is_some() {
                    let index = entries.len();
                    entries.push((offset, OnceCell::new()));
                    addresses.extend(ranges.map(|range| (range, index)));
                }
            }
            Ok(())
        })?;
        Ok(Self {
            entries,
            addresses: AddressIndex::new(addresses),
        })
    }
}

struct FunctionFrame
{
    offset: UnitOffset<usize>,
    name: Option<String>,
    call_site: Option<CallSite>,
}

struct FunctionFrames
{
    frames: Vec<FunctionFrame>,
    children: Vec<AddressIndex<usize>>,
}

impl FunctionFrames
{
    fn covering(&self, pc: u64) -> Vec<&FunctionFrame>
    {
        let mut index = 0;
        let mut frames = vec![&self.frames[index]];
        while let Some(&child) = self.children[index].covering(pc).next() {
            index = child;
            frames.push(&self.frames[index]);
        }
        frames
    }
}

/// The physical owner and the outermost-to-innermost DIEs selected from that unit.
pub(crate) struct DwarfFrames<'a>
{
    pub(crate) unit: Unit<'a>,
    pub(crate) owner: UnitOffset<usize>,
    frames: Vec<&'a FunctionFrame>,
    record: &'a FrameUnit,
}

impl DwarfFrames<'_>
{
    pub(crate) fn scope(&self, kind: FrameKind) -> Option<UnitOffset<usize>>
    {
        let named = self.frames.iter().filter(|frame| frame.name.is_some());
        match kind {
            // The outermost named entry appears as physical, even if its DIE is inline.
            FrameKind::Physical => Some(named.map(|frame| frame.offset).next().unwrap_or(self.owner)),
            FrameKind::Inlined { depth, .. } => named.rev().nth(usize::from(depth)).map(|frame| frame.offset),
        }
    }

    pub(super) fn symbols(&self, pc: u64) -> Vec<SymbolFrame>
    {
        let lines = self.record.lines(self.unit.dwarf);
        let mut location = lines.at(pc);
        self.frames
            .iter()
            .rev()
            .filter_map(|frame| {
                let current = location.take();
                location = frame.call_site.as_ref().and_then(|call| lines.location(call));
                Some(SymbolFrame {
                    symbol: make_symbol_name(frame.name.clone()?, SymbolSource::DebugInfo),
                    location: current,
                    parameters: Vec::new(),
                })
            })
            .collect()
    }
}

struct CallSite
{
    file: u64,
    line: Option<u32>,
    column: Option<u32>,
}

impl CallSite
{
    fn read(unit: Unit<'_>, entry: &Die<'_, '_>) -> gimli::Result<Option<Self>>
    {
        let Some(AttributeValue::FileIndex(file)) = entry.attr_value(gimli::DW_AT_call_file)? else {
            return Ok(None);
        };
        if file == 0 && unit.header.version() < 5 {
            return Ok(None);
        }
        let number = |attribute| -> gimli::Result<Option<u32>> {
            Ok(entry
                .attr(attribute)?
                .and_then(|attr| attr.udata_value())
                .and_then(|value| u32::try_from(value).ok())
                .filter(|&value| value != 0))
        };
        Ok(Some(Self {
            file,
            line: number(gimli::DW_AT_call_line)?,
            column: number(gimli::DW_AT_call_column)?,
        }))
    }
}

struct FrameLines
{
    files: Vec<Option<String>>,
    sequences: AddressIndex<LineSequence>,
}

impl FrameUnit
{
    fn lines(&self, dwarf: &OwnedDwarf) -> &FrameLines
    {
        self.lines.get_or_init(|| {
            let mut sequences = Vec::new();
            let (header, result) =
                read_sequences(&self.unit, &mut sequences, |_, file| Ok(file.try_into().unwrap_or(u32::MAX)));
            if let Err(error) = result {
                tracing::debug!(unit = self.offset.0, %error, "unreadable frame line program");
            }
            let files = header.map_or_else(Vec::new, |header| {
                let count = header.file_names().len() + usize::from(header.version() < 5);
                (0..count.max(1) as u64)
                    .map(|index| match header.file(index) {
                        Some(file) => render_file(dwarf, &self.unit, file, &header).ok(),
                        None if index == 0 => Some(String::new()),
                        None => None,
                    })
                    .collect()
            });
            FrameLines {
                files,
                sequences: AddressIndex::new(sequences.into_iter().map(|sequence| {
                    (
                        Range {
                            begin: sequence.start,
                            end: sequence.end,
                        },
                        sequence,
                    )
                })),
            }
        })
    }
}

impl FrameLines
{
    fn at(&self, pc: u64) -> Option<SourceLocation>
    {
        let sequence = self.sequences.covering(pc).next()?;
        let end = sequence.rows.partition_point(|row| row.address <= pc);
        let row = sequence.rows.get(end.checked_sub(1)?)?;
        self.location(&CallSite {
            file: u64::from(row.file),
            line: (row.line != 0).then_some(row.line),
            column: (row.line != 0).then_some(row.column),
        })
    }

    fn location(&self, position: &CallSite) -> Option<SourceLocation>
    {
        let index = usize::try_from(position.file).ok()?;
        let file = self.files.get(index)?.as_ref()?.clone();
        Some(SourceLocation {
            file,
            line: position.line,
            column: position.column,
        })
    }
}
