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
//! All public lookups return runtime [`Address`]es; the index stores file virtual addresses
//! and translates through the image mapping.

use std::collections::{BTreeSet, HashMap};

use gimli::{AttributeValue, DwAt, Reader as _, Unit, UnitSectionOffset, constants};

use super::image::BinaryImage;
use super::lines::{LineIndex, SourceLookupError};
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
    /// Runtime address of the function's first instruction (`DW_AT_low_pc`, or the lowest
    /// start of its `DW_AT_ranges`).
    pub entry: Address,
    /// Runtime address of the first instruction after the prologue: `entry` when the entry
    /// row itself is flagged `prologue_end` (no prologue); otherwise the first line-table
    /// row after `entry` and before `end` flagged `prologue_end`; otherwise the first row
    /// after `entry` whose line differs from the entry row's line; otherwise `entry`.
    pub after_prologue: Address,
    /// Runtime end (exclusive) of the address range that contains `entry`.
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

/// The concrete functions of one image, by the last segment of their path.
#[derive(Debug, Default)]
pub(crate) struct FunctionIndex
{
    functions: Vec<FunctionEntry>,
    by_last: HashMap<String, Vec<usize>>,
    /// Sorted, distinct entry addresses of every indexed function.
    starts: Vec<u64>,
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
    fn build(dwarf: &OwnedDwarf) -> Result<Self, SourceLookupError>
    {
        let mut names = HashMap::<usize, DieName>::new();
        // (own DIE offset, entry, end) of every concrete subprogram.
        let mut concrete = Vec::<(usize, u64, u64)>::new();
        let mut units = dwarf.units();
        while let Some(header) = units.next().map_err(dwarf_error)? {
            let base = match header.offset() {
                UnitSectionOffset::DebugInfoOffset(offset) => offset.0,
                UnitSectionOffset::DebugTypesOffset(_) => continue,
            };
            let unit = dwarf.unit(header).map_err(dwarf_error)?;
            let mut scopes = Vec::<Option<String>>::new();
            let mut depth = 0_isize;
            let mut entries = unit.entries();
            while let Some((delta, entry)) = entries.next_dfs().map_err(dwarf_error)? {
                depth += delta;
                scopes.truncate(usize::try_from(depth).unwrap_or(0));
                let tag = entry.tag();
                let own_name = match entry.attr_value(constants::DW_AT_name).map_err(dwarf_error)? {
                    Some(value) => Some(attr_string(dwarf, &unit, value)?),
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
                        .attr_value(constants::DW_AT_linkage_name)
                        .map_err(dwarf_error)?
                        .or(entry.attr_value(constants::DW_AT_MIPS_linkage_name).map_err(dwarf_error)?)
                    {
                        Some(value) => Some(attr_string(dwarf, &unit, value)?),
                        None => None,
                    };
                    let reference = reference(entry, constants::DW_AT_specification, base)?.or(reference(
                        entry,
                        constants::DW_AT_abstract_origin,
                        base,
                    )?);
                    names.insert(
                        offset,
                        DieName {
                            linkage,
                            scoped,
                            reference,
                        },
                    );
                    let declaration = matches!(
                        entry.attr_value(constants::DW_AT_declaration).map_err(dwarf_error)?,
                        Some(AttributeValue::Flag(true))
                    );
                    if !declaration && let Some((start, end)) = entry_range(dwarf, &unit, entry)? {
                        concrete.push((offset, start, end));
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
                scopes.push(scope_name);
            }
        }

        let mut functions: Vec<FunctionEntry> = concrete
            .into_iter()
            .filter_map(|(offset, entry, end)| {
                let name = resolve_name(&names, offset)?;
                let path = normalize(&name)?;
                Some(FunctionEntry { name, path, entry, end })
            })
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
        Ok(Self {
            functions,
            by_last,
            starts,
        })
    }

    /// Sorted, distinct entry addresses (file virtual addresses) of every indexed function.
    pub(crate) fn starts(&self) -> &[u64]
    {
        &self.starts
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

/// The name of subprogram `offset`: its own linkage name, demangled, or that of the DIE
/// it refers to; otherwise the first scoped `DW_AT_name` along the same chain.
fn resolve_name(names: &HashMap<usize, DieName>, offset: usize) -> Option<String>
{
    let mut scoped = None;
    let mut next = Some(offset);
    // A short bound guards against reference cycles in malformed DWARF.
    for _ in 0..8 {
        let die = names.get(&next?)?;
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
) -> Result<Option<usize>, SourceLookupError>
{
    Ok(match entry.attr_value(name).map_err(dwarf_error)? {
        Some(AttributeValue::UnitRef(offset)) => Some(base + offset.0),
        Some(AttributeValue::DebugInfoRef(offset)) => Some(offset.0),
        _ => None,
    })
}

/// `(entry, end)` of a DIE with code: the lowest non-empty range start and the end of
/// that range. `None` when the DIE has no non-empty range (or only ranges at address 0,
/// left behind by dead-stripped code).
fn entry_range(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    entry: &gimli::DebuggingInformationEntry<'_, '_, OwnedReader>,
) -> Result<Option<(u64, u64)>, SourceLookupError>
{
    let mut best: Option<(u64, u64)> = None;
    let mut ranges = dwarf.die_ranges(unit, entry).map_err(dwarf_error)?;
    while let Some(range) = ranges.next().map_err(dwarf_error)? {
        if range.begin == 0 || range.begin >= range.end {
            continue;
        }
        if best.is_none_or(|(start, _)| range.begin < start) {
            best = Some((range.begin, range.end));
        }
    }
    Ok(best)
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

fn attr_string(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    value: AttributeValue<OwnedReader>,
) -> Result<String, SourceLookupError>
{
    let string = dwarf.attr_string(unit, value).map_err(dwarf_error)?;
    Ok(string.to_string_lossy().map_err(dwarf_error)?.into_owned())
}

fn dwarf_error(error: gimli::Error) -> SourceLookupError
{
    SourceLookupError::DebugInfo(error.to_string())
}

impl BinaryImage
{
    /// The image's function index, built from every compilation unit on first use.
    pub(crate) fn function_index(&self) -> Result<&FunctionIndex, SourceLookupError>
    {
        self.function_index_cell().get_or_try_init(|| {
            let dwarf = self
                .dwarf()
                .map_err(|error| SourceLookupError::DebugInfo(error.to_string()))?;
            FunctionIndex::build(dwarf)
        })
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
    /// [`SourceLookupError::DebugInfo`] when the DWARF cannot be read.
    pub fn function_addresses(&self, name: &str) -> Result<Vec<FunctionAddress>, SourceLookupError>
    {
        let matches = self.function_index()?.lookup(name)?;
        let lines = self.line_index()?;
        Ok(matches
            .into_iter()
            .filter_map(|function| {
                Some(FunctionAddress {
                    name: function.name.clone(),
                    path: function.path.join("::"),
                    entry: self.runtime_address(function.entry)?,
                    after_prologue: self.runtime_address(after_prologue(lines, function.entry, function.end))?,
                    end: self.runtime_address(function.end - 1)? + 1,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

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
            by_last,
            starts: Vec::new(),
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
