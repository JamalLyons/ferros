//! Function index: function name to entry and post-prologue addresses.
//!
//! The index is built once per [`BinaryImage`], on the first lookup, from the concrete
//! `DW_TAG_subprogram` DIEs of every compilation unit: those with code (`DW_AT_low_pc` and
//! `DW_AT_high_pc`, or `DW_AT_ranges`) that are not declarations. Abstract-only DIEs
//! (functions that exist only as inlined copies) have no code and are not indexed. A
//! concrete DIE's name comes from its own attributes or, through
//! `DW_AT_specification` / `DW_AT_abstract_origin`, from the DIE it refers to.
//!
//! # Names and paths
//!
//! A function's display name is its demangled linkage name (`DW_AT_linkage_name`, Rust v0
//! or legacy, demangled without the hash), for example
//! `ferros_fixture_symbols::generic_identity::<u64>` or `<my_crate::Counter>::bump`.
//! Without a linkage name, it is the enclosing namespace and type names joined with `::`,
//! followed by `DW_AT_name`.
//!
//! The display name is normalized to a path of `::` segments for matching:
//!
//! - generic arguments are removed (`generic_identity::<u64>` -> `generic_identity`,
//!   `<alloc::vec::Vec<T>>::push` -> `alloc::vec::Vec::push`);
//! - an inherent-impl qualifier `<Type>::m` becomes `Type`'s path followed by `m`;
//! - a trait-impl qualifier `<Type as Trait>::m` also becomes `Type`'s path followed by
//!   `m`: a trait method matches through its self type (`Counter::fmt`), not through the
//!   trait path. References, raw pointers and `dyn` in front of the self type are dropped;
//!   a slice, array or tuple self type stays one segment (`[u8]`).
//!
//! A query is normalized the same way. It matches a function when its segments equal the
//! trailing segments of the function's path, so a full path (`my_crate::shapes::area`) and
//! any trailing part of it (`shapes::area`, `area`, `Counter::bump`) match. When the matches
//! have more than one distinct path, the lookup is ambiguous, unless exactly one of those
//! paths equals the whole query, which then wins (so `main` picks a function whose whole
//! path is `main`, such as a C entry point, over `my_crate::main`).
//!
//! A function's entry is `DW_AT_low_pc`, else `DW_AT_entry_pc`, else the start of the first
//! range in `DW_AT_ranges`, and its end is the end of the range holding the entry. With
//! hot/cold splitting, the first range is the hot (entry) part and can lie above the cold
//! part, so the lowest address is not used.
//!
//! Separately from entries, the index keeps every non-empty range of every concrete
//! function (its fragments: the hot and cold parts of a split function, for example), which
//! the line index uses to tell functions apart within one line-table sequence.
//!
//! # Unreadable debug info
//!
//! A compilation unit whose header or root entry cannot be parsed is skipped. A unit whose
//! line-program header alone cannot be parsed is still indexed: `gimli` reads that header
//! while loading the unit, so such a unit is loaded again without its line program, which
//! functions do not need (the line index still skips and reports it). A DIE whose
//! attributes, strings or range list cannot be read is skipped alone; when the DIE tree
//! itself cannot be walked further, the rest of that unit is skipped. Every other unit is
//! still indexed, and each skip is recorded and reported as for the line index (see
//! [`BinaryImage::skipped_debug_info`] and the `lines` module documentation). The built
//! index is cached, and so is a failure to load the DWARF sections at all.
//!
//! All public lookups return runtime [`Address`]es; the index stores file virtual addresses
//! and translates through the image mapping.
//!
//! # Map of this file
//!
//! - `FunctionAddress`: a lookup result.
//! - `FunctionIndex`: built by `FunctionIndex::build` from `index_die`, `resolve_name`,
//!   `function_code`, `entry_base`, and `after_prologue`.
//! - Name normalization: `normalize`, `normalize_type`, `split_top`, `strip_generics`.
//! - `impl BinaryImage`: `function_index`, `function_addresses`, `function_containing`.
//! - Tests in `mod tests`.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use gimli::{
    AttributeValue, DebugAddrBase, DebugLocListsBase, DebugRngListsBase, DebugStrOffsetsBase, DwAt, Reader as _, Unit,
    UnitHeader, UnitSectionOffset, UnitType, constants,
};

use super::image::BinaryImage;
use super::lines::{LineIndex, SkippedDebugInfo, SourceLookupError, warn_skipped};
use super::{OwnedDwarf, OwnedReader};
use crate::types::Address;

/// One function matched by [`BinaryImage::function_addresses`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionAddress
{
    /// Demangled name, generic arguments included where the linkage name has them
    /// (`my_crate::generic_identity::<u64>`, `<my_crate::Counter>::bump`).
    pub name: String,
    /// Normalized path the query was matched against, segments joined with `::`
    /// (`my_crate::generic_identity`, `my_crate::Counter::bump`).
    pub path: String,
    /// Runtime address of the function's entry: `DW_AT_low_pc`, else `DW_AT_entry_pc`, else
    /// the start of the first range listed in `DW_AT_ranges`. For a function split into hot
    /// and cold parts this is the entry of the hot part, which need not be its lowest
    /// address.
    pub entry: Address,
    /// Runtime address of the first instruction after the prologue: `entry` when the entry
    /// row itself is flagged `prologue_end` (no prologue); otherwise the first line-table
    /// row after `entry` and before `end` flagged `prologue_end`; otherwise the first row
    /// after `entry` whose line differs from the entry row's line; otherwise `entry`.
    pub after_prologue: Address,
    /// Runtime end (exclusive) of the address range that contains `entry` (for a split
    /// function, the end of the part that holds the entry only).
    pub end: Address,
}

/// One indexed function (file virtual addresses).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FunctionEntry
{
    name: String,
    path: Vec<String>,
    entry: u64,
    end: u64,
}

/// The code of one concrete subprogram (file virtual addresses).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FunctionCode
{
    entry: u64,
    /// End of the range that holds `entry`.
    end: u64,
    /// Every non-empty range, in listed order.
    ranges: Vec<Range<u64>>,
}

/// The concrete functions of one image, by the last segment of their path.
#[derive(Debug, Default)]
pub(crate) struct FunctionIndex
{
    functions: Vec<FunctionEntry>,
    /// Every concrete function by entry, including those whose name does not normalize to
    /// a path (`path` is then empty): what [`containing`](Self::containing) searches.
    by_address: Vec<FunctionEntry>,
    by_last: HashMap<String, Vec<usize>>,
    /// Sorted, distinct entry addresses of every indexed function.
    starts: Vec<u64>,
    /// Every non-empty range of every concrete function, sorted and distinct.
    fragments: Vec<Range<u64>>,
    /// Units (or entries of them) that could not be parsed and were left out.
    skipped: Vec<SkippedDebugInfo>,
}

/// Name attributes of one subprogram DIE.
#[derive(Debug, Default)]
struct DieName
{
    linkage: Option<String>,
    /// Enclosing namespace and type names plus `DW_AT_name`, joined with `::`.
    scoped: Option<String>,
    /// Global `.debug_info` offset of the `DW_AT_specification` / `DW_AT_abstract_origin`.
    reference: Option<usize>,
}

impl FunctionIndex
{
    /// Index the concrete subprograms of every compilation unit that can be parsed,
    /// skipping the others (see the module documentation).
    pub(crate) fn build(dwarf: &OwnedDwarf) -> Self
    {
        let mut names = HashMap::<usize, DieName>::new();
        // (own DIE offset, code) of every concrete subprogram.
        let mut concrete = Vec::<(usize, FunctionCode)>::new();
        let mut skipped = Vec::new();
        let mut units = dwarf.units();
        loop {
            let header = match units.next() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                Err(error) => {
                    // Without a readable header, the next unit's offset is unknown.
                    skipped.push(SkippedDebugInfo::new(None, "unit header", error));
                    break;
                }
            };
            let UnitSectionOffset::DebugInfoOffset(base) = header.offset() else {
                continue;
            };
            let base = base.0;
            let offset = Some(base as u64);
            // A unit whose line-program header does not parse still has readable functions.
            let unit = match dwarf.unit(header.clone()).or_else(|_| unit_without_lines(dwarf, header)) {
                Ok(unit) => unit,
                Err(error) => {
                    skipped.push(SkippedDebugInfo::new(offset, "unit", error));
                    continue;
                }
            };
            let mut scopes = Vec::<Option<String>>::new();
            let mut depth = 0_isize;
            let mut entries = unit.entries();
            loop {
                let (delta, entry) = match entries.next_dfs() {
                    Ok(Some(next)) => next,
                    Ok(None) => break,
                    Err(error) => {
                        // The tree cannot be walked past an entry that does not parse.
                        skipped.push(SkippedDebugInfo::new(offset, "DIE tree", error));
                        break;
                    }
                };
                depth += delta;
                scopes.truncate(usize::try_from(depth).unwrap_or(0));
                let die_offset = base + entry.offset().0;
                let scope_name = match index_die(dwarf, &unit, entry, base, &scopes, &mut names, &mut concrete) {
                    Ok(scope_name) => scope_name,
                    Err(error) => {
                        // Only this entry is lost; its children still get a scope slot.
                        skipped.push(SkippedDebugInfo::new(
                            offset,
                            &format!("DIE at .debug_info+{die_offset:#x}"),
                            error,
                        ));
                        None
                    }
                };
                scopes.push(scope_name);
            }
        }

        // Fragments of every concrete function, named or not.
        let mut fragments: Vec<Range<u64>> = concrete.iter().flat_map(|(_, code)| code.ranges.iter().cloned()).collect();
        fragments.sort_by_key(|range| (range.start, range.end));
        fragments.dedup();
        let mut by_address: Vec<FunctionEntry> = concrete
            .into_iter()
            .filter_map(|(offset, code)| {
                let name = resolve_name(&names, offset)?;
                let path = normalize(&name).unwrap_or_default();
                Some(FunctionEntry {
                    name,
                    path,
                    entry: code.entry,
                    end: code.end,
                })
            })
            .collect();
        by_address.sort_by(|a, b| (a.entry, &a.path, &a.name).cmp(&(b.entry, &b.path, &b.name)));
        by_address.dedup_by(|a, b| a.entry == b.entry && a.path == b.path);
        let mut functions: Vec<FunctionEntry> = by_address
            .iter()
            .filter(|function| !function.path.is_empty())
            .cloned()
            .collect();
        functions.sort_by(|a, b| (a.entry, &a.path, &a.name).cmp(&(b.entry, &b.path, &b.name)));
        functions.dedup_by(|a, b| a.entry == b.entry && a.path == b.path);
        let mut by_last = HashMap::<String, Vec<usize>>::new();
        for (at, function) in functions.iter().enumerate() {
            if let Some(last) = function.path.last() {
                by_last.entry(last.clone()).or_default().push(at);
            }
        }
        let mut starts: Vec<u64> = functions.iter().map(|function| function.entry).collect();
        starts.dedup();
        Self {
            functions,
            by_address,
            by_last,
            starts,
            fragments,
            skipped,
        }
    }

    /// Debug info left out of this index because it could not be parsed.
    pub(crate) fn skipped(&self) -> &[SkippedDebugInfo]
    {
        &self.skipped
    }

    /// Sorted, distinct entry addresses (file virtual addresses) of every indexed function.
    pub(crate) fn starts(&self) -> &[u64]
    {
        &self.starts
    }

    /// Every non-empty range (file virtual addresses) of every concrete function, named or
    /// not, sorted and distinct: one per part of a function split into hot and cold parts.
    pub(crate) fn fragments(&self) -> &[Range<u64>]
    {
        &self.fragments
    }

    /// The function whose entry range (the part holding its entry) contains `address` (a
    /// file virtual address), whether or not its name parses as a path; the one with the
    /// latest entry when ranges nest.
    fn containing(&self, address: u64) -> Option<&FunctionEntry>
    {
        let below = self.by_address.partition_point(|function| function.entry <= address);
        self.by_address[..below].iter().rev().find(|function| address < function.end)
    }

    /// Every function matching `query` (see the module documentation), sorted by entry.
    fn lookup(&self, query: &str) -> Result<Vec<&FunctionEntry>, SourceLookupError>
    {
        let invalid = || SourceLookupError::InvalidFunctionName { query: query.to_owned() };
        let wanted = normalize(query).ok_or_else(invalid)?;
        let last = wanted.last().ok_or_else(invalid)?;
        let mut matches: Vec<&FunctionEntry> = self
            .by_last
            .get(last)
            .into_iter()
            .flatten()
            .map(|&at| &self.functions[at])
            .filter(|function| function.path.ends_with(&wanted))
            .collect();
        let paths: BTreeSet<&[String]> = matches.iter().map(|function| function.path.as_slice()).collect();
        if paths.is_empty() {
            return Err(SourceLookupError::UnknownFunction { query: query.to_owned() });
        }
        if paths.len() > 1 {
            if !paths.contains(wanted.as_slice()) {
                return Err(SourceLookupError::AmbiguousFunction {
                    query: query.to_owned(),
                    candidates: paths.iter().map(|path| path.join("::")).collect(),
                });
            }
            matches.retain(|function| function.path == wanted);
        }
        matches.sort_by_key(|function| function.entry);
        Ok(matches)
    }
}

/// Load a unit as `gimli::Dwarf::unit` does, but without its line program (and without the
/// unit's `DW_AT_name` and `DW_AT_comp_dir`, which only the line program uses): the root
/// entry's `DW_AT_low_pc` and section bases, which addresses, strings and range lists of
/// its DIEs depend on, are read the same way.
fn unit_without_lines(dwarf: &OwnedDwarf, header: UnitHeader<OwnedReader>) -> gimli::Result<Unit<OwnedReader>>
{
    let encoding = header.encoding();
    let mut unit = Unit {
        abbreviations: dwarf.abbreviations(&header)?,
        name: None,
        comp_dir: None,
        low_pc: 0,
        str_offsets_base: DebugStrOffsetsBase::default_for_encoding_and_file(encoding, dwarf.file_type),
        addr_base: DebugAddrBase(0),
        loclists_base: DebugLocListsBase::default_for_encoding_and_file(encoding, dwarf.file_type),
        rnglists_base: DebugRngListsBase::default_for_encoding_and_file(encoding, dwarf.file_type),
        line_program: None,
        dwo_id: match header.type_() {
            UnitType::Skeleton(dwo_id) | UnitType::SplitCompilation(dwo_id) => Some(dwo_id),
            _ => None,
        },
        header,
    };
    let mut low_pc = None;
    {
        let mut cursor = unit.header.entries(&unit.abbreviations);
        cursor.next_dfs()?;
        let root = cursor.current().ok_or(gimli::Error::MissingUnitDie)?;
        let mut attrs = root.attrs();
        while let Some(attr) = attrs.next()? {
            match (attr.name(), attr.value()) {
                (constants::DW_AT_low_pc, value) => low_pc = Some(value),
                (constants::DW_AT_str_offsets_base, AttributeValue::DebugStrOffsetsBase(base)) => {
                    unit.str_offsets_base = base;
                }
                (constants::DW_AT_addr_base | constants::DW_AT_GNU_addr_base, AttributeValue::DebugAddrBase(base)) => {
                    unit.addr_base = base;
                }
                (constants::DW_AT_loclists_base, AttributeValue::DebugLocListsBase(base)) => unit.loclists_base = base,
                (
                    constants::DW_AT_rnglists_base | constants::DW_AT_GNU_ranges_base,
                    AttributeValue::DebugRngListsBase(base),
                ) => unit.rnglists_base = base,
                _ => {}
            }
        }
    }
    if let Some(value) = low_pc
        && let Some(address) = dwarf.attr_address(&unit, value)?
    {
        unit.low_pc = address;
    }
    Ok(unit)
}

/// Record one DIE: a subprogram's names in `names` and, when it has code and is not a
/// declaration, its offset and code in `concrete`. Returns the name the DIE gives to the
/// scope of its children (`Some` for a namespace or a named type).
fn index_die(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    entry: &gimli::DebuggingInformationEntry<'_, '_, OwnedReader>,
    base: usize,
    scopes: &[Option<String>],
    names: &mut HashMap<usize, DieName>,
    concrete: &mut Vec<(usize, FunctionCode)>,
) -> gimli::Result<Option<String>>
{
    let tag = entry.tag();
    let own_name = match entry.attr_value(constants::DW_AT_name)? {
        Some(value) => Some(attr_string(dwarf, unit, value)?),
        None => None,
    };
    if tag == constants::DW_TAG_subprogram {
        let offset = base + entry.offset().0;
        let scope: Vec<&str> = scopes.iter().flatten().map(String::as_str).collect();
        let scoped = own_name.as_ref().map(|name| {
            let mut path = scope.join("::");
            if !path.is_empty() {
                path.push_str("::");
            }
            path.push_str(name);
            path
        });
        let linkage = match entry
            .attr_value(constants::DW_AT_linkage_name)?
            .or(entry.attr_value(constants::DW_AT_MIPS_linkage_name)?)
        {
            Some(value) => Some(attr_string(dwarf, unit, value)?),
            None => None,
        };
        let reference = reference(entry, constants::DW_AT_specification, base)?.or(reference(
            entry,
            constants::DW_AT_abstract_origin,
            base,
        )?);
        let declaration = matches!(
            entry.attr_value(constants::DW_AT_declaration)?,
            Some(AttributeValue::Flag(true))
        );
        let code = if declaration {
            None
        } else {
            function_code(dwarf, unit, entry)?
        };
        names.insert(
            offset,
            DieName {
                linkage,
                scoped,
                reference,
            },
        );
        if let Some(code) = code {
            concrete.push((offset, code));
        }
    }
    let scope_name = matches!(
        tag,
        constants::DW_TAG_namespace
            | constants::DW_TAG_structure_type
            | constants::DW_TAG_enumeration_type
            | constants::DW_TAG_union_type
            | constants::DW_TAG_class_type
    )
    .then_some(own_name)
    .flatten();
    Ok(scope_name)
}

/// The name of subprogram `offset`: its own linkage name, demangled, or that of the DIE
/// it refers to; otherwise the first scoped `DW_AT_name` along the same chain.
fn resolve_name(names: &HashMap<usize, DieName>, offset: usize) -> Option<String>
{
    let mut scoped = None;
    let mut next = Some(offset);
    // A short bound guards against reference cycles in malformed DWARF.
    for _ in 0..8 {
        // The chain ends at a DIE without a reference, or at one that was not indexed (in a
        // skipped unit, for example); a name found on the way is still used.
        let Some(die) = next.and_then(|offset| names.get(&offset)) else {
            break;
        };
        if let Some(linkage) = &die.linkage {
            return Some(rustc_demangle::try_demangle(linkage).map_or_else(|_| linkage.clone(), |name| format!("{name:#}")));
        }
        if scoped.is_none() {
            scoped.clone_from(&die.scoped);
        }
        next = die.reference;
    }
    scoped
}

/// The global `.debug_info` offset an attribute refers to.
fn reference(
    entry: &gimli::DebuggingInformationEntry<'_, '_, OwnedReader>,
    name: DwAt,
    base: usize,
) -> gimli::Result<Option<usize>>
{
    Ok(match entry.attr_value(name)? {
        Some(AttributeValue::UnitRef(offset)) => Some(base + offset.0),
        Some(AttributeValue::DebugInfoRef(offset)) => Some(offset.0),
        _ => None,
    })
}

/// The code of a DIE: entry, end and non-empty ranges. The entry is `DW_AT_low_pc`, else `DW_AT_entry_pc`,
/// else the start of the first listed non-empty range (not the lowest: with hot/cold
/// splitting the lowest range can be the cold part). `end` is the end of the non-empty
/// range that contains the entry; an attribute entry outside every such range is ignored.
/// `None` when the DIE has no non-empty range (or only ranges at address 0, left behind by
/// dead-stripped code).
fn function_code(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    entry: &gimli::DebuggingInformationEntry<'_, '_, OwnedReader>,
) -> gimli::Result<Option<FunctionCode>>
{
    let mut ranges = Vec::new();
    let mut iter = dwarf.die_ranges(unit, entry)?;
    while let Some(range) = iter.next()? {
        if range.begin != 0 && range.begin < range.end {
            ranges.push(range);
        }
    }
    let Some(&first) = ranges.first() else {
        return Ok(None);
    };
    let code = |entry, end| FunctionCode {
        entry,
        end,
        ranges: ranges.iter().map(|range| range.begin..range.end).collect(),
    };
    let containing = |start: u64| {
        ranges
            .iter()
            .find(|range| range.begin <= start && start < range.end)
            .map(|range| code(start, range.end))
    };
    for name in [constants::DW_AT_low_pc, constants::DW_AT_entry_pc] {
        let Some(value) = entry.attr_value(name)? else {
            continue;
        };
        let start = match (dwarf.attr_address(unit, value.clone())?, value.udata_value()) {
            (Some(address), _) => Some(address),
            // DWARF 5 allows a constant `DW_AT_entry_pc`: an offset from the beginning
            // address of the function.
            (None, Some(offset)) if name == constants::DW_AT_entry_pc => {
                Some(entry_base(dwarf, unit, entry, &ranges)?.wrapping_add(offset))
            }
            (None, _) => None,
        };
        // An attribute entry outside every non-empty range (an address of 0 left by
        // dead-stripped code, for example) is not trusted.
        if let Some(found) = start.and_then(containing) {
            return Ok(Some(found));
        }
    }
    Ok(Some(code(first.begin, first.end)))
}

/// The address a DWARF 5 constant `DW_AT_entry_pc` is relative to: the DIE's
/// `DW_AT_low_pc`, else the lowest start of its non-empty `ranges`.
fn entry_base(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    entry: &gimli::DebuggingInformationEntry<'_, '_, OwnedReader>,
    ranges: &[gimli::Range],
) -> gimli::Result<u64>
{
    if let Some(value) = entry.attr_value(constants::DW_AT_low_pc)?
        && let Some(address) = dwarf.attr_address(unit, value)?
    {
        return Ok(address);
    }
    Ok(ranges.iter().map(|range| range.begin).min().unwrap_or(0))
}

/// The first address after the prologue of the function at `[entry, end)` (see
/// [`FunctionAddress::after_prologue`]).
fn after_prologue(lines: &LineIndex, entry: u64, end: u64) -> u64
{
    let Some((sequence, at)) = lines.row_at(entry) else {
        return entry;
    };
    let entry_row = sequence.rows[at];
    // A function without a prologue (optimized leaf code) has `prologue_end` on its entry.
    if entry_row.address == entry && entry_row.prologue_end {
        return entry;
    }
    let entry_line = entry_row.line;
    let body = || {
        sequence.rows[at + 1..]
            .iter()
            .take_while(|row| row.address < end && !row.end_sequence)
            .filter(|row| row.address > entry)
    };
    body()
        .find(|row| row.prologue_end)
        .or_else(|| body().find(|row| row.line != 0 && row.line != entry_line))
        .map_or(entry, |row| row.address)
}

/// Normalize a demangled function name or a query to path segments (see the module
/// documentation). `None` for an empty or malformed name: an empty segment, unbalanced
/// brackets, or nothing left after removing generic arguments.
fn normalize(name: &str) -> Option<Vec<String>>
{
    let mut path = Vec::new();
    for (at, segment) in split_top(name.trim())?.into_iter().enumerate() {
        let segment = segment.trim();
        if segment.is_empty() {
            return None;
        }
        if segment.starts_with('<') {
            if group_end(segment)? != segment.len() - 1 {
                return None;
            }
            if at == 0 {
                // `<Type>` or `<Type as Trait>`: match through the self type.
                let inner = &segment[1..segment.len() - 1];
                let self_type = find_top(inner, " as ").map_or(inner, |split| &inner[..split]);
                path.extend(normalize_type(self_type)?);
            }
            // Elsewhere, a `<...>` segment is a turbofish generic argument list: dropped.
            continue;
        }
        let plain = strip_generics(segment)?;
        if plain.is_empty() {
            return None;
        }
        path.push(plain);
    }
    (!path.is_empty()).then_some(path)
}

/// Path segments of a self type: references, pointers and `dyn` dropped; a slice, array
/// or tuple kept as one segment.
fn normalize_type(mut ty: &str) -> Option<Vec<String>>
{
    loop {
        let trimmed = ty.trim_start();
        let stripped = ["&", "mut ", "*const ", "*mut ", "dyn "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix));
        match stripped {
            Some(rest) => ty = rest,
            None => {
                ty = trimmed.trim_end();
                break;
            }
        }
    }
    if ty.starts_with('[') || ty.starts_with('(') {
        return (!ty.is_empty()).then(|| vec![ty.to_owned()]);
    }
    normalize(ty)
}

/// Split on `::` outside `<>`, `()` and `[]`. `None` when the brackets do not balance.
fn split_top(name: &str) -> Option<Vec<&str>>
{
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    let bytes = name.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' | b'(' | b'[' => depth += 1,
            // `->` in a function type is not a closing bracket.
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => depth = depth.checked_sub(1)?,
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => {
                parts.push(&name[start..at]);
                at += 2;
                start = at;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    (depth == 0).then(|| {
        parts.push(&name[start..]);
        parts
    })
}

/// Byte index of the bracket closing the one that opens `text`.
fn group_end(text: &str) -> Option<usize>
{
    let bytes = text.as_bytes();
    let mut depth = 0_usize;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// Byte index of the first `needle` outside brackets.
fn find_top(text: &str, needle: &str) -> Option<usize>
{
    let bytes = text.as_bytes();
    let mut depth = 0_usize;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => depth = depth.saturating_sub(1),
            _ if depth == 0 && text[at..].starts_with(needle) => return Some(at),
            _ => {}
        }
    }
    None
}

/// A segment with every `<...>` group removed (`Vec<T>` -> `Vec`). `None` when unbalanced.
fn strip_generics(segment: &str) -> Option<String>
{
    let mut plain = String::new();
    let mut depth = 0_usize;
    let mut previous = '\0';
    for c in segment.chars() {
        match c {
            '<' => depth += 1,
            '>' if previous == '-' => {}
            '>' => depth = depth.checked_sub(1)?,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
        previous = c;
    }
    (depth == 0).then(|| plain.trim().to_owned())
}

fn attr_string(dwarf: &OwnedDwarf, unit: &Unit<OwnedReader>, value: AttributeValue<OwnedReader>) -> gimli::Result<String>
{
    Ok(dwarf.attr_string(unit, value)?.to_string_lossy()?.into_owned())
}

impl BinaryImage
{
    /// The image's function index, built on first use from every compilation unit that can
    /// be parsed. The result is cached, including a failure to load the DWARF sections (see
    /// the module documentation).
    pub(crate) fn function_index(&self) -> Result<&FunctionIndex, SourceLookupError>
    {
        self.function_index_cell()
            .get_or_init(|| {
                let dwarf = self
                    .dwarf()
                    .map_err(|error| SourceLookupError::DebugInfo(error.to_string()))?;
                let index = FunctionIndex::build(dwarf);
                warn_skipped(self, "function", index.skipped());
                Ok(index)
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Every function whose path matches `name`, with its entry and post-prologue
    /// addresses, sorted by entry.
    ///
    /// `name` is a Rust path, matched on `::` segment boundaries against the trailing
    /// segments of each function's normalized path: `my_crate::shapes::area`,
    /// `shapes::area` and `area` all match `fn area` in `mod shapes`, and `Counter::bump`
    /// matches the inherent method `<my_crate::Counter>::bump`. Generic arguments are
    /// ignored, so a generic function returns one entry per instantiation. A trait-impl
    /// method matches through its self type (`Counter::fmt` for
    /// `<my_crate::Counter as core::fmt::Display>::fmt`). Functions that were only ever
    /// inlined have no entry and are not found. See the module documentation for the
    /// full rules.
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::InvalidFunctionName`] for an empty or malformed `name`,
    /// [`SourceLookupError::UnknownFunction`] when nothing matches,
    /// [`SourceLookupError::AmbiguousFunction`] when the matches have more than one
    /// distinct path and none of them equals the whole of `name`, and
    /// [`SourceLookupError::DebugInfo`] when the DWARF sections cannot be loaded. Units and
    /// entries that cannot be parsed are skipped rather than failing the lookup (see
    /// [`BinaryImage::skipped_debug_info`]).
    pub fn function_addresses(&self, name: &str) -> Result<Vec<FunctionAddress>, SourceLookupError>
    {
        let matches = self.function_index()?.lookup(name)?;
        let lines = self.line_index()?;
        Ok(matches
            .into_iter()
            .filter_map(|function| self.function_address(lines, function))
            .collect())
    }

    /// The function whose code holds `address`, found by address rather than by name, so
    /// trait-impl methods, closures and names shared by several functions are found too.
    ///
    /// Only the part of a function that holds its entry counts (for a function split into
    /// hot and cold parts, an address in the cold part finds nothing). When ranges nest,
    /// the function with the latest entry at or before `address` wins. Functions that were
    /// only ever inlined have no code of their own and are not found.
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::DebugInfo`] when the DWARF sections cannot be loaded.
    pub fn function_containing(&self, address: Address) -> Result<Option<FunctionAddress>, SourceLookupError>
    {
        let Some(file_address) = self.file_address(address) else {
            return Ok(None);
        };
        let lines = self.line_index()?;
        Ok(self
            .function_index()?
            .containing(file_address)
            .and_then(|function| self.function_address(lines, function)))
    }

    fn function_address(&self, lines: &LineIndex, function: &FunctionEntry) -> Option<FunctionAddress>
    {
        Some(FunctionAddress {
            name: function.name.clone(),
            path: function.path.join("::"),
            entry: self.runtime_address(function.entry)?,
            after_prologue: self.runtime_address(after_prologue(lines, function.entry, function.end))?,
            end: self.runtime_address(function.end - 1)? + 1,
        })
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    /// AC1 (issue #88, L4): a unit that does not parse is skipped; functions of the good
    /// unit are still found and the skip is recorded.
    #[test]
    fn a_corrupt_unit_does_not_hide_the_good_one()
    {
        let (dwarf, bad_offset) = super::super::lines::synthetic::corrupt_then_good();
        let index = FunctionIndex::build(&dwarf);
        let found = index.lookup("good_fn").expect("good_fn");
        assert_eq!(found.iter().map(|function| function.entry).collect::<Vec<_>>(), vec![0x2000]);
        assert!(matches!(
            index.lookup("bad_fn"),
            Err(SourceLookupError::UnknownFunction { .. })
        ));
        assert_eq!(index.skipped().len(), 1, "{:?}", index.skipped());
        assert_eq!(index.skipped()[0].unit_offset, Some(bad_offset));
    }

    /// AC3 (issue #88, L7): the hot range listed first is the entry even though the cold
    /// range listed after it is lower; `DW_AT_entry_pc` wins over the first range.
    #[test]
    fn split_function_entry_is_the_first_listed_range()
    {
        use super::super::lines::synthetic::{TestUnit, write};

        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.functions = vec![
            ("split", vec![(0x3000, 0x3040), (0x1000, 0x1010)], None),
            ("with_entry_pc", vec![(0x5000, 0x5010), (0x4000, 0x4020)], Some(0x4008)),
            // Dead-stripped ranges (at address 0) are ignored.
            ("stripped", vec![(0, 0x10), (0x6100, 0x6110)], None),
        ];
        let dwarf = write(&[unit]).load();
        let index = FunctionIndex::build(&dwarf);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        let found = |name| {
            let found = index.lookup(name).expect(name);
            assert_eq!(found.len(), 1, "{name}");
            (found[0].entry, found[0].end)
        };
        assert_eq!(found("split"), (0x3000, 0x3040));
        assert_eq!(found("with_entry_pc"), (0x4008, 0x4020));
        assert_eq!(found("stripped"), (0x6100, 0x6110));
        assert_eq!(index.starts(), &[0x3000, 0x4008, 0x6100]);
        assert_eq!(
            index.fragments(),
            &[0x1000..0x1010, 0x3000..0x3040, 0x4000..0x4020, 0x5000..0x5010, 0x6100..0x6110]
        );
    }

    /// Issue #93: a unit whose line-program header does not parse and whose `.debug_info`
    /// is sound still has its functions indexed; the line index skips the unit.
    #[test]
    fn a_bad_line_program_header_keeps_the_units_functions()
    {
        use super::super::lines::synthetic::{TestUnit, write};

        let mut bad = TestUnit::new("/w", "src/bad.rs");
        bad.rows = vec![(0x1000, 3), (0x1004, 4)];
        bad.end = 0x1010;
        bad.functions = vec![("in_bad_lines", vec![(0x1000, 0x1010)], None)];
        let mut good = TestUnit::new("/w", "src/good.rs");
        good.rows = vec![(0x2000, 7)];
        good.end = 0x2010;
        good.functions = vec![("in_good_lines", vec![(0x2000, 0x2010)], None)];
        let mut written = write(&[bad, good]);
        written.corrupt_line_header(0);
        let dwarf = written.load();

        let index = FunctionIndex::build(&dwarf);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        let found = index.lookup("in_bad_lines").expect("in_bad_lines");
        assert_eq!((found[0].entry, found[0].end), (0x1000, 0x1010));
        assert_eq!(index.lookup("in_good_lines").expect("in_good_lines")[0].entry, 0x2000);

        let lines = LineIndex::build(&dwarf, index.fragments());
        assert_eq!(lines.skipped().len(), 1, "{:?}", lines.skipped());
        assert_eq!(lines.skipped()[0].unit_offset, Some(0));
        assert!(lines.skipped()[0].reason.starts_with("unit: "), "{:?}", lines.skipped());
    }

    /// One function's code, looked up by name: `(entry, end)`.
    fn code_of(index: &FunctionIndex, name: &str) -> (u64, u64)
    {
        let found = index.lookup(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(found.len(), 1, "{name}");
        (found[0].entry, found[0].end)
    }

    /// Issue #93: a DWARF 5 constant `DW_AT_entry_pc` is an offset from `DW_AT_low_pc`, or,
    /// without one, from the lowest start of the DIE's ranges (not the first listed).
    #[test]
    fn a_constant_entry_pc_is_relative_to_the_entry_base()
    {
        use gimli::write::{Address as WriteAddress, AttributeValue as Write};
        use gimli::{DW_AT_entry_pc, DW_AT_low_pc};

        use super::super::lines::synthetic::{TestUnit, write};

        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.functions = vec![
            // Lowest start 0x1000: 0x1000 + 0x2008 is in the hot part. Relative to the
            // first listed range it would be 0x5008, outside every range.
            ("from_ranges", vec![(0x3000, 0x3040), (0x1000, 0x1010)], None),
            // `DW_AT_low_pc` 0x5000 (outside the ranges, so not the entry itself):
            // 0x5000 + 0x2008 is in the hot part. Relative to the lowest start (0x6000) it
            // would be 0x8008, outside every range.
            ("from_low_pc", vec![(0x7000, 0x7020), (0x6000, 0x6010)], None),
        ];
        unit.attributes = vec![
            ("from_ranges", DW_AT_entry_pc, Write::Udata(0x2008)),
            ("from_low_pc", DW_AT_low_pc, Write::Address(WriteAddress::Constant(0x5000))),
            ("from_low_pc", DW_AT_entry_pc, Write::Udata(0x2008)),
        ];
        let dwarf = write(&[unit]).load();
        let index = FunctionIndex::build(&dwarf);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        assert_eq!(code_of(&index, "from_ranges"), (0x3008, 0x3040));
        assert_eq!(code_of(&index, "from_low_pc"), (0x7008, 0x7020));
    }

    /// Issue #93: a `DW_AT_low_pc` or `DW_AT_entry_pc` outside every non-empty range is not
    /// the entry: the next attribute is tried, then the first listed range is used.
    #[test]
    fn an_entry_outside_every_range_falls_back_to_the_first_range()
    {
        use gimli::DW_AT_low_pc;
        use gimli::write::{Address as WriteAddress, AttributeValue as Write};

        use super::super::lines::synthetic::{TestUnit, write};

        let address = |address| Write::Address(WriteAddress::Constant(address));
        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.functions = vec![
            ("bad_low_pc", vec![(0x8000, 0x8010), (0x7800, 0x7810)], None),
            ("bad_entry_pc", vec![(0x9000, 0x9010), (0x8800, 0x8810)], Some(0x9999)),
            ("bad_low_good_entry", vec![(0xa000, 0xa010), (0x9800, 0x9810)], Some(0x9808)),
        ];
        unit.attributes = vec![
            ("bad_low_pc", DW_AT_low_pc, address(0x8888)),
            ("bad_low_good_entry", DW_AT_low_pc, address(0x1)),
        ];
        let dwarf = write(&[unit]).load();
        let index = FunctionIndex::build(&dwarf);
        assert!(index.skipped().is_empty(), "{:?}", index.skipped());
        assert_eq!(code_of(&index, "bad_low_pc"), (0x8000, 0x8010));
        assert_eq!(code_of(&index, "bad_entry_pc"), (0x9000, 0x9010));
        assert_eq!(code_of(&index, "bad_low_good_entry"), (0x9808, 0x9810));
    }

    /// Issue #93: a DIE whose attributes cannot be read is skipped alone; the functions
    /// around it are indexed.
    #[test]
    fn an_unreadable_die_is_skipped_alone()
    {
        use gimli::write::AttributeValue as Write;
        use gimli::{DW_AT_name, DebugStrOffset};

        use super::super::lines::synthetic::{TestUnit, write};

        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.functions = vec![
            ("before", vec![(0x1000, 0x1010)], None),
            ("unreadable", vec![(0x2000, 0x2010)], None),
            ("after", vec![(0x3000, 0x3010)], None),
        ];
        // A string in a supplementary object file that is not there.
        unit.attributes = vec![("unreadable", DW_AT_name, Write::DebugStrRefSup(DebugStrOffset(0)))];
        let written = write(&[unit]);
        let dwarf = written.load();
        let index = FunctionIndex::build(&dwarf);
        assert_eq!(code_of(&index, "before"), (0x1000, 0x1010));
        assert_eq!(code_of(&index, "after"), (0x3000, 0x3010));
        assert!(matches!(
            index.lookup("unreadable"),
            Err(SourceLookupError::UnknownFunction { .. })
        ));
        assert_eq!(index.skipped().len(), 1, "{:?}", index.skipped());
        assert_eq!(index.skipped()[0].unit_offset, Some(0));
        // The reported DIE lies between its readable neighbours.
        let reason = &index.skipped()[0].reason;
        let die = reason
            .strip_prefix("DIE at .debug_info+0x")
            .and_then(|rest| rest.split_once(':'))
            .and_then(|(hex, _)| usize::from_str_radix(hex, 16).ok())
            .unwrap_or_else(|| panic!("{reason:?}"));
        assert!(
            written.subprogram_offset("before") < die && die < written.subprogram_offset("after"),
            "{reason:?}"
        );
    }

    /// Issue #93: when the DIE tree cannot be walked past an entry, the functions before it
    /// are kept, the rest of the unit is skipped, and later units are indexed.
    #[test]
    fn an_unwalkable_die_tree_skips_the_rest_of_the_unit()
    {
        use super::super::lines::synthetic::{TestUnit, write};

        let mut first = TestUnit::new("/w", "src/lib.rs");
        first.functions = vec![
            ("before", vec![(0x1000, 0x1010)], None),
            ("broken", vec![(0x2000, 0x2010)], None),
            ("after", vec![(0x3000, 0x3010)], None),
        ];
        let mut second = TestUnit::new("/w", "src/other.rs");
        second.functions = vec![("next_unit", vec![(0x4000, 0x4010)], None)];
        let mut written = write(&[first, second]);
        let broken = written.subprogram_offset("broken");
        // An abbreviation code that is not in the table.
        written.corrupt_info(broken, 0x7f);
        let dwarf = written.load();
        let index = FunctionIndex::build(&dwarf);
        assert_eq!(code_of(&index, "before"), (0x1000, 0x1010));
        assert_eq!(code_of(&index, "next_unit"), (0x4000, 0x4010));
        for lost in ["broken", "after"] {
            assert!(
                matches!(index.lookup(lost), Err(SourceLookupError::UnknownFunction { .. })),
                "{lost}"
            );
        }
        assert_eq!(index.skipped().len(), 1, "{:?}", index.skipped());
        assert_eq!(index.skipped()[0].unit_offset, Some(0));
        assert!(index.skipped()[0].reason.starts_with("DIE tree: "), "{:?}", index.skipped());
    }

    /// Issue #93: a unit header that does not parse stops indexing, with a skip that has no
    /// unit offset, in both indexes; units before it are kept.
    #[test]
    fn an_unreadable_unit_header_stops_indexing()
    {
        use super::super::lines::synthetic::{TestUnit, write};

        let mut first = TestUnit::new("/w", "src/first.rs");
        first.rows = vec![(0x1000, 3)];
        first.end = 0x1010;
        first.functions = vec![("in_first", vec![(0x1000, 0x1010)], None)];
        let mut second = TestUnit::new("/w", "src/second.rs");
        second.rows = vec![(0x2000, 5)];
        second.end = 0x2010;
        second.functions = vec![("in_second", vec![(0x2000, 0x2010)], None)];
        let mut written = write(&[first, second]);
        written.corrupt_unit_header(1);
        let dwarf = written.load();

        let index = FunctionIndex::build(&dwarf);
        assert_eq!(code_of(&index, "in_first"), (0x1000, 0x1010));
        assert!(matches!(
            index.lookup("in_second"),
            Err(SourceLookupError::UnknownFunction { .. })
        ));
        let lines = LineIndex::build(&dwarf, index.fragments());
        for skipped in [index.skipped(), lines.skipped()] {
            assert_eq!(skipped.len(), 1, "{skipped:?}");
            assert_eq!(skipped[0].unit_offset, None);
            assert!(skipped[0].reason.starts_with("unit header: "), "{skipped:?}");
        }
        // The same skip from both indexes is reported once.
        assert_eq!(
            super::super::lines::merge_skipped(lines.skipped(), index.skipped()),
            index.skipped()
        );
    }

    /// A function with no linkage name is named by its scoped `DW_AT_name`, also when the
    /// DIE it refers to was not indexed (it is in a skipped unit).
    #[test]
    fn a_name_without_linkage_survives_a_missing_reference()
    {
        let die = |scoped: Option<&str>, reference| DieName {
            linkage: None,
            scoped: scoped.map(str::to_owned),
            reference,
        };
        let names = HashMap::from([
            (0x10, die(Some("ns::plain"), None)),
            (0x20, die(Some("ns::dangling"), Some(0x999))),
            (0x30, die(None, Some(0x10))),
            (0x40, die(None, None)),
        ]);
        assert_eq!(resolve_name(&names, 0x10).as_deref(), Some("ns::plain"));
        assert_eq!(resolve_name(&names, 0x20).as_deref(), Some("ns::dangling"));
        assert_eq!(resolve_name(&names, 0x30).as_deref(), Some("ns::plain"));
        assert_eq!(resolve_name(&names, 0x40), None);
        assert_eq!(resolve_name(&names, 0x50), None);
    }

    fn path(name: &str) -> Option<String>
    {
        normalize(name).map(|segments| segments.join("::"))
    }

    #[test]
    fn names_normalize_to_matchable_paths()
    {
        assert_eq!(
            path("krate::generic_identity::<u64>").as_deref(),
            Some("krate::generic_identity")
        );
        assert_eq!(path("<krate::Counter>::bump").as_deref(), Some("krate::Counter::bump"));
        assert_eq!(
            path("<krate::Counter as core::fmt::Display>::fmt").as_deref(),
            Some("krate::Counter::fmt")
        );
        assert_eq!(path("<alloc::vec::Vec<u8>>::push").as_deref(), Some("alloc::vec::Vec::push"));
        assert_eq!(
            path("<&mut krate::Counter as core::fmt::Debug>::fmt").as_deref(),
            Some("krate::Counter::fmt")
        );
        assert_eq!(path("<[u8] as core::fmt::Debug>::fmt").as_deref(), Some("[u8]::fmt"));
        assert_eq!(
            path("krate::apply::<fn() -> u8, u8>::{closure#0}").as_deref(),
            Some("krate::apply::{closure#0}")
        );
        assert_eq!(path("ns::Type<T>::method").as_deref(), Some("ns::Type::method"));
        assert_eq!(path("main").as_deref(), Some("main"));
        for bad in ["", "  ", "::area", "area::", "a::::b", "<a", "a<b", "a>b", "<a>x::b"] {
            assert_eq!(path(bad), None, "{bad:?}");
        }
    }

    /// Issue #120 (3): a function is found by address whatever its name looks like, and
    /// only through the part of its code that holds its entry.
    #[test]
    fn a_function_is_found_by_an_address_in_its_entry_part()
    {
        use super::super::lines::synthetic::{TestUnit, write};

        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.functions = vec![
            ("<my_crate::Counter as my_crate::Bump>::bump", vec![(0x1000, 0x1020)], None),
            ("my_crate::main::{{closure}}", vec![(0x1020, 0x1040)], None),
            ("split", vec![(0x3000, 0x3040), (0x2000, 0x2010)], None),
            ("<my_crate::Gate<'<'> as my_crate::Bump>::bump", vec![(0x4000, 0x4010)], None),
        ];
        let dwarf = write(&[unit]).load();
        let index = FunctionIndex::build(&dwarf);
        assert!(
            index.lookup("Gate::bump").is_err(),
            "a bracket in a const argument does not parse as a path"
        );
        let name = |address| index.containing(address).map(|function| function.name.as_str());
        assert_eq!(name(0x1000), Some("<my_crate::Counter as my_crate::Bump>::bump"));
        assert_eq!(name(0x101c), Some("<my_crate::Counter as my_crate::Bump>::bump"));
        assert_eq!(name(0x1020), Some("my_crate::main::{{closure}}"));
        assert_eq!(name(0x3010), Some("split"));
        assert_eq!(name(0x2004), None, "a cold part is not the entry part");
        assert_eq!(name(0x4008), Some("<my_crate::Gate<'<'> as my_crate::Bump>::bump"));
        assert_eq!(name(0x0fff), None);
        assert_eq!(name(0x1040), None);
    }

    #[test]
    fn lookup_matches_trailing_segments_and_prefers_an_exact_path()
    {
        let entry = |name: &str, entry| FunctionEntry {
            name: name.to_owned(),
            path: normalize(name).expect("valid"),
            entry,
            end: entry + 4,
        };
        let functions = vec![
            entry("main", 0x10),
            entry("krate::main", 0x20),
            entry("krate::id::<u64>", 0x40),
            entry("krate::id::<u32>", 0x30),
            entry("other::id", 0x50),
        ];
        let mut by_last = HashMap::<String, Vec<usize>>::new();
        for (at, function) in functions.iter().enumerate() {
            by_last
                .entry(function.path.last().expect("path").clone())
                .or_default()
                .push(at);
        }
        let index = FunctionIndex {
            functions,
            by_address: Vec::new(),
            by_last,
            starts: Vec::new(),
            fragments: Vec::new(),
            skipped: Vec::new(),
        };
        let entries = |query| {
            index
                .lookup(query)
                .map(|found| found.iter().map(|function| function.entry).collect::<Vec<_>>())
        };
        assert_eq!(entries("main"), Ok(vec![0x10]));
        assert_eq!(entries("krate::main"), Ok(vec![0x20]));
        assert_eq!(entries("krate::id"), Ok(vec![0x30, 0x40]));
        assert_eq!(
            entries("id"),
            Err(SourceLookupError::AmbiguousFunction {
                query: "id".into(),
                candidates: vec!["krate::id".into(), "other::id".into()],
            })
        );
        assert_eq!(
            entries("rate::id"),
            Err(SourceLookupError::UnknownFunction {
                query: "rate::id".into()
            })
        );
        assert_eq!(
            entries(""),
            Err(SourceLookupError::InvalidFunctionName { query: String::new() })
        );
    }
}
