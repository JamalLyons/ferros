//! Source line tables: `file:line` to addresses, and address to line and ranges.
//!
//! The index is built once per [`BinaryImage`], on the first lookup, from the line program
//! of every compilation unit.
//!
//! # File paths
//!
//! A line-table file is rendered from its compilation directory, include directory, and file
//! name. An absolute component replaces the preceding path. For line lookups, the path is
//! then normalized: empty and `.` components are dropped and the
//! remaining components are joined with `/` (a leading `/` is kept for an absolute path).
//! Files are keyed by that normalized path, so units that spell one file differently
//! (`/w/./src/lib.rs` and `/w/src/lib.rs`) share one entry, and [`LineAddresses::file`] /
//! [`AddressLine::file`] report the normalized spelling. For the usual paths, which have no
//! empty or `.` components, that is exactly the path [`BinaryImage::symbolicate`] reports;
//! otherwise it can differ from it in those components only. `..` components are kept as
//! they are: resolving them without the file system is unsound when a directory is a
//! symbolic link.
//!
//! # Unreadable debug info
//!
//! A compilation unit whose header, root entry or line-program header cannot be parsed is
//! skipped whole (reported as `unit: ...`). When the rows of its line program stop parsing,
//! the rest of that program is skipped (reported as `line program: ...`) and the sequences
//! completed before the error are kept. The index is built from every other unit. If the list of unit headers itself cannot be read
//! past some point, later units cannot be located and indexing stops there. Each skip is
//! recorded as a [`SkippedDebugInfo`], logged as a `tracing` warning, and reported by
//! [`BinaryImage::skipped_debug_info`], so a caller that gets
//! [`SourceLookupError::UnknownFile`] can tell missing debug info from a file that has no
//! line information.
//!
//! The built index is cached on the image, and so is a failure to build it at all (the
//! image's DWARF sections cannot be loaded): such a failure comes from the image's bytes,
//! which do not change, so repeating the attempt on every lookup could only fail again.
//!
//! All public lookups take and return runtime [`Address`]es; the index itself stores file
//! virtual addresses and translates through the image mapping.
//!
//! # Map of this file
//!
//! - Errors and skips: `SourceLookupError`, `SkippedDebugInfo`, `merge_skipped`, `warn_skipped`.
//! - Results: `LineAddresses`, `AddressLine`.
//! - `LineIndex` (`LineRow`, `LineSequence`): built by `LineIndex::build`; `row_at`.
//! - Path rendering and normalization: `render_file`, `normalize_path`.
//! - `impl BinaryImage`: `line_index`, `skipped_debug_info`, `line_addresses`,
//!   `line_at_address`, `statement_at_address`.
//! - Synthetic DWARF writer for tests, then `mod tests`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use gimli::{AttributeValue, ColumnType, FileEntry, LineProgramHeader, Reader as _, Unit, UnitHeader};

use super::image::BinaryImage;
use super::{OwnedDwarf, OwnedReader};
use crate::types::Address;

/// Failure to translate a source location or a function name to addresses, or to read
/// the debug info.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceLookupError
{
    /// The requested line is not a valid 1-based line number (line 0).
    #[error("invalid line {line}: source lines start at 1")]
    InvalidLine
    {
        /// The rejected line.
        line: u32,
    },

    /// No source file in the image's line tables matches the requested path.
    ///
    /// When [`BinaryImage::skipped_debug_info`] is not empty, the file may be in debug info
    /// that could not be read.
    #[error("no source file matching `{query}` has line information")]
    UnknownFile
    {
        /// The requested path, as given.
        query: String,
    },

    /// The requested path matches more than one distinct source file.
    #[error("`{query}` matches more than one source file: {}", candidates.join(", "))]
    AmbiguousFile
    {
        /// The requested path, as given.
        query: String,
        /// Every matching full path, sorted.
        candidates: Vec<String>,
    },

    /// The file has no code on the requested line or on any later line.
    #[error("{file} has no code at or after line {line} (last line with code: {last_line})")]
    NoCodeAtOrAfterLine
    {
        /// The full path of the matched file.
        file: String,
        /// The requested line.
        line: u32,
        /// The last line of the file that has code.
        last_line: u32,
    },

    /// A function name is empty or malformed (an empty `::` segment or unbalanced
    /// brackets), as rejected by [`BinaryImage::function_addresses`].
    #[error("invalid function name `{query}`")]
    InvalidFunctionName
    {
        /// The rejected name, as given.
        query: String,
    },

    /// No function with code in the image matches the requested name.
    ///
    /// When [`BinaryImage::skipped_debug_info`] is not empty, the function may be in debug
    /// info that could not be read.
    #[error("no function matching `{query}`")]
    UnknownFunction
    {
        /// The requested name, as given.
        query: String,
    },

    /// The requested name matches functions with more than one distinct path, and none of
    /// those paths equals the whole name.
    #[error("`{query}` matches more than one function: {}", candidates.join(", "))]
    AmbiguousFunction
    {
        /// The requested name, as given.
        query: String,
        /// Every matching normalized path, sorted and distinct.
        candidates: Vec<String>,
    },

    /// The image's DWARF sections could not be loaded at all. Individual compilation units
    /// that cannot be parsed are skipped instead (see [`BinaryImage::skipped_debug_info`]).
    #[error("failed to read debug info: {0}")]
    DebugInfo(String),
}

/// Debug info that was skipped, because it could not be parsed, while building an image's
/// line or function index, as returned by [`BinaryImage::skipped_debug_info`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct SkippedDebugInfo
{
    /// `.debug_info` offset of the compilation unit that was skipped (or that contains the
    /// skipped entry), or `None` when the list of unit headers could not be read past this
    /// point, so no later unit was indexed.
    pub unit_offset: Option<u64>,
    /// What was skipped and the parse error, for example `line program: ...` or
    /// `DIE at .debug_info+0x1f4: ...`.
    pub reason: String,
}

impl SkippedDebugInfo
{
    pub(crate) fn new(unit_offset: Option<u64>, context: &str, error: gimli::Error) -> Self
    {
        Self {
            unit_offset,
            reason: format!("{context}: {error}"),
        }
    }
}

/// `.debug_info` offset of a unit, `None` for a `.debug_types` unit.
pub(crate) fn unit_offset(header: &UnitHeader<OwnedReader>) -> Option<u64>
{
    header.offset().as_debug_info_offset().map(|offset| offset.0 as u64)
}

/// The statement addresses of one source line, as returned by [`BinaryImage::line_addresses`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAddresses
{
    /// Full path of the matched source file, normalized (see the module documentation).
    pub file: String,
    /// The line the addresses belong to: the requested line, or the next line with code.
    pub line: u32,
    /// One runtime address per contiguous run of line-table rows for `line` (one per
    /// inlined copy or generic instantiation, typically), sorted and de-duplicated.
    pub addresses: Vec<Address>,
}

/// The source line containing an address, as returned by [`BinaryImage::line_at_address`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressLine
{
    /// Full path of the source file, normalized (see the module documentation).
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, or `None` when the row has no column (the left edge).
    pub column: Option<u32>,
    /// Every runtime `[start, end)` range in the address's line-table sequence whose rows
    /// map to the same file and line, adjacent ranges merged, sorted by start.
    pub ranges: Vec<Range<Address>>,
}

/// One line-table row (file virtual address).
///
/// Rows that share an address are collapsed into the last one, preserving any
/// `prologue_end` flag from the collapsed rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineRow
{
    pub(crate) address: u64,
    /// Index into [`LineIndex::files`].
    pub(crate) file: u32,
    /// 0 for compiler-generated code with no source line.
    pub(crate) line: u32,
    /// 0 for the left edge.
    pub(crate) column: u32,
    pub(crate) is_stmt: bool,
    pub(crate) prologue_end: bool,
    pub(crate) end_sequence: bool,
}

/// One line-table sequence: contiguous rows ending with an `end_sequence` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineSequence
{
    /// File virtual address range `[start, end)`.
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// Rows sorted by address; the last one is the `end_sequence` row at `end`.
    pub(crate) rows: Vec<LineRow>,
}

/// Line tables of one image, indexed both ways.
#[derive(Debug, Default)]
pub(crate) struct LineIndex
{
    /// Distinct normalized file paths (see the module documentation) of the line-table
    /// file entries that rows refer to.
    pub(crate) files: Vec<String>,
    /// Every non-empty sequence, sorted by start address.
    pub(crate) sequences: Vec<LineSequence>,
    /// Per file (same index as `files`): line -> canonical statement file addresses.
    lines: Vec<BTreeMap<u32, Vec<u64>>>,
    /// Sorted, distinct starts and ends of every non-empty range of every concrete function
    /// (its fragments: both parts of a function split into hot and cold parts). One
    /// line-table sequence can hold several functions back to back, so runs and line ranges
    /// stop at these.
    boundaries: Vec<u64>,
    /// Units (or parts of them) that could not be parsed and were left out.
    skipped: Vec<SkippedDebugInfo>,
}

impl LineIndex
{
    /// Index every compilation unit's line program that can be parsed, skipping the others
    /// (see the module documentation). `fragments` are the non-empty ranges of the image's
    /// concrete functions.
    pub(crate) fn build(dwarf: &OwnedDwarf, fragments: &[Range<u64>]) -> Self
    {
        let mut index = Self::with_fragments(fragments);
        let mut paths = HashMap::<String, u32>::new();
        let mut units = dwarf.units();
        loop {
            let header = match units.next() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                Err(error) => {
                    // Without a readable header, the next unit's offset is unknown.
                    index.skipped.push(SkippedDebugInfo::new(None, "unit header", error));
                    break;
                }
            };
            let offset = unit_offset(&header);
            let unit = match dwarf.unit(header) {
                Ok(unit) => unit,
                Err(error) => {
                    index.skipped.push(SkippedDebugInfo::new(offset, "unit", error));
                    continue;
                }
            };
            if let Err(error) = index.add_unit(dwarf, &unit, &mut paths) {
                index.skipped.push(SkippedDebugInfo::new(offset, "line program", error));
            }
        }

        let mut files = vec![String::new(); paths.len()];
        for (path, file) in paths {
            files[file as usize] = path;
        }
        index.files = files;
        index.sequences.sort_by_key(|sequence| (sequence.start, sequence.end));
        index.lines = vec![BTreeMap::new(); index.files.len()];
        for sequence in &index.sequences {
            index_statements(&mut index.lines, sequence, &index.boundaries);
        }
        index
    }

    /// Add the sequences of one unit's line program. On a parse error, the sequences
    /// completed before it are kept.
    fn add_unit(
        &mut self,
        dwarf: &OwnedDwarf,
        unit: &Unit<OwnedReader>,
        paths: &mut HashMap<String, u32>,
    ) -> gimli::Result<()>
    {
        // DWARF file index -> index into files, for this unit.
        let mut unit_files = HashMap::<u64, u32>::new();
        let (_, result) = read_sequences(unit, &mut self.sequences, |header, file_index| {
            let file = match unit_files.get(&file_index) {
                Some(&file) => file,
                None => {
                    let path = header
                        .file(file_index)
                        .map(|entry| render_file(dwarf, unit, entry, header))
                        .transpose()?
                        .map(|path| normalize_path(&path))
                        .unwrap_or_default();
                    let next = u32::try_from(paths.len()).unwrap_or(u32::MAX);
                    let file = *paths.entry(path).or_insert(next);
                    unit_files.insert(file_index, file);
                    file
                }
            };
            Ok(file)
        });
        result
    }

    /// Debug info left out of this index because it could not be parsed.
    pub(crate) fn skipped(&self) -> &[SkippedDebugInfo]
    {
        &self.skipped
    }

    /// An empty index that splits sequences at the starts and ends of `fragments`.
    fn with_fragments(fragments: &[Range<u64>]) -> Self
    {
        let mut boundaries: Vec<u64> = fragments.iter().flat_map(|range| [range.start, range.end]).collect();
        boundaries.sort_unstable();
        boundaries.dedup();
        Self {
            boundaries,
            ..Self::default()
        }
    }

    /// `[start, end)` of the function fragment containing `address`: from the closest
    /// fragment boundary (start or end) at or below it to the next boundary (unbounded when
    /// there is none). For an address in a cold part, that is the cold part's range.
    fn function_bounds(&self, address: u64) -> Range<u64>
    {
        let next = self.boundaries.partition_point(|&boundary| boundary <= address);
        let start = next.checked_sub(1).map_or(0, |at| self.boundaries[at]);
        let end = self.boundaries.get(next).copied().unwrap_or(u64::MAX);
        start..end
    }

    /// Resolve a user-supplied path to one file index.
    fn resolve_file(&self, query: &str) -> Result<u32, SourceLookupError>
    {
        let wanted = components(query);
        let absolute = query.starts_with('/');
        let mut candidates = BTreeSet::new();
        for (file, path) in self.files.iter().enumerate() {
            if self.lines[file].is_empty() || wanted.is_empty() {
                continue;
            }
            let have = components(path);
            if absolute && path.starts_with('/') && have == wanted {
                // An exact absolute match wins over any other suffix match.
                return Ok(file as u32);
            }
            if !absolute && have.ends_with(&wanted) {
                candidates.insert(file as u32);
            }
        }
        let mut found = candidates.iter();
        match (found.next(), found.next()) {
            (Some(&file), None) => Ok(file),
            (None, _) => Err(SourceLookupError::UnknownFile { query: query.to_owned() }),
            (Some(_), Some(_)) => {
                let mut paths: Vec<String> = candidates.iter().map(|&file| self.files[file as usize].clone()).collect();
                paths.sort();
                Err(SourceLookupError::AmbiguousFile {
                    query: query.to_owned(),
                    candidates: paths,
                })
            }
        }
    }

    /// `(file, line)` of each row of `sequence` as a line step sees it: a row that is not
    /// `is_stmt` takes the key of the closest `is_stmt` row before it in the same function
    /// fragment, if any. No statement carries across a fragment boundary.
    fn statement_keys(&self, sequence: &LineSequence) -> Vec<((u32, u32), u32)>
    {
        let mut current: Option<((u32, u32), u32)> = None;
        let mut fragment = None;
        sequence
            .rows
            .iter()
            .map(|row| {
                let bounds = self.function_bounds(row.address);
                if fragment.as_ref() != Some(&bounds) {
                    current = None;
                    fragment = Some(bounds);
                }
                if row.is_stmt {
                    current = Some(((row.file, row.line), row.column));
                }
                current.unwrap_or(((row.file, row.line), row.column))
            })
            .collect()
    }

    /// The statement key and column of row `at` of `sequence` (see
    /// [`statement_keys`](Self::statement_keys)); `None` for line 0.
    fn statement_at(&self, sequence: &LineSequence, at: usize) -> Option<((u32, u32), u32)>
    {
        let (key, column) = self.statement_keys(sequence)[at];
        (key.1 != 0).then_some((key, column))
    }

    /// The ranges of row `at`'s statement within the function fragment of `address`.
    fn statement_ranges(&self, sequence: &LineSequence, at: usize, address: u64) -> Vec<Range<u64>>
    {
        let keys: Vec<(u32, u32)> = self.statement_keys(sequence).into_iter().map(|(key, _)| key).collect();
        line_ranges(sequence, &keys, keys[at], self.function_bounds(address))
    }

    /// The sequence and row index whose `[row, next row)` range contains `address`.
    pub(crate) fn row_at(&self, address: u64) -> Option<(&LineSequence, usize)>
    {
        let sequence = self
            .sequences
            .iter()
            .find(|sequence| sequence.start <= address && address < sequence.end)?;
        let row = sequence.rows.partition_point(|row| row.address <= address).checked_sub(1)?;
        Some((sequence, row))
    }
}

/// Read and collapse rows for one unit. Return the header with any dynamically defined
/// files, and keep completed sequences even if a later row fails to parse.
pub(super) fn read_sequences(
    unit: &Unit<OwnedReader>,
    sequences: &mut Vec<LineSequence>,
    mut file_index: impl FnMut(&LineProgramHeader<OwnedReader>, u64) -> gimli::Result<u32>,
) -> (Option<LineProgramHeader<OwnedReader>>, gimli::Result<()>)
{
    let Some(program) = unit.line_program.clone() else {
        return (None, Ok(()));
    };
    let mut rows = program.rows();
    let result = (|| {
        let mut current = Vec::<LineRow>::new();
        while let Some((header, row)) = rows.next_row()? {
            let file = file_index(header, row.file_index())?;
            let line = LineRow {
                address: row.address(),
                file,
                line: row.line().map_or(0, |line| u32::try_from(line.get()).unwrap_or(u32::MAX)),
                column: match row.column() {
                    ColumnType::LeftEdge => 0,
                    ColumnType::Column(column) => u32::try_from(column.get()).unwrap_or(u32::MAX),
                },
                is_stmt: row.is_stmt(),
                prologue_end: row.prologue_end(),
                end_sequence: row.end_sequence(),
            };
            match current.last_mut() {
                Some(last) if last.address == line.address && !last.end_sequence => {
                    let prologue_end = last.prologue_end;
                    *last = line;
                    last.prologue_end |= prologue_end;
                }
                _ => current.push(line),
            }
            if line.end_sequence {
                let rows = std::mem::take(&mut current);
                if rows.len() > 1 {
                    sequences.push(LineSequence {
                        start: rows[0].address,
                        end: line.address,
                        rows,
                    });
                }
            }
        }
        Ok(())
    })();
    (Some(rows.header().clone()), result)
}

/// Record the canonical statement address of every run of rows for one line.
///
/// A run is a maximal stretch of consecutive rows with the same file and line within one
/// function fragment; its canonical address is its first `is_stmt` row. Rows with line 0
/// end a run, and a fragment boundary (`boundaries`, sorted) starts a new one, so functions
/// or fragments laid out back to back in one sequence (two instantiations of a one-line
/// generic, two cold parts) each get their own address.
fn index_statements(lines: &mut [BTreeMap<u32, Vec<u64>>], sequence: &LineSequence, boundaries: &[u64])
{
    let mut run: Option<(u32, u32)> = None;
    let mut recorded = false;
    for row in sequence.rows.iter().filter(|row| !row.end_sequence) {
        let key = (row.file, row.line);
        if run != Some(key) || boundaries.binary_search(&row.address).is_ok() {
            run = Some(key);
            recorded = false;
        }
        if row.line != 0 && row.is_stmt && !recorded {
            recorded = true;
            let addresses = lines[row.file as usize].entry(row.line).or_default();
            if let Err(at) = addresses.binary_search(&row.address) {
                addresses.insert(at, row.address);
            }
        }
    }
}

/// Path components, ignoring empty and `.` components; `/` and `\` both separate.
fn components(path: &str) -> Vec<&str>
{
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

/// A rendered path's [`components`] joined with `/`, with a leading `/` when the path is
/// absolute. `..` components are kept.
pub(crate) fn normalize_path(path: &str) -> String
{
    let joined = components(path).join("/");
    if path.starts_with('/') { format!("/{joined}") } else { joined }
}

/// Render a line-program file entry for both frame and source lookups.
pub(super) fn render_file(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    file: &FileEntry<OwnedReader>,
    header: &LineProgramHeader<OwnedReader>,
) -> gimli::Result<String>
{
    let mut path = match &unit.comp_dir {
        Some(dir) => dir.to_string_lossy()?.into_owned(),
        None => String::new(),
    };
    // Directory index 0 is the compilation directory itself.
    if file.directory_index() != 0
        && let Some(directory) = file.directory(header)
    {
        push_path(&mut path, &attr_string(dwarf, unit, directory)?);
    }
    push_path(&mut path, &attr_string(dwarf, unit, file.path_name())?);
    Ok(path)
}

fn attr_string(dwarf: &OwnedDwarf, unit: &Unit<OwnedReader>, value: AttributeValue<OwnedReader>) -> gimli::Result<String>
{
    Ok(dwarf.attr_string(unit, value)?.to_string_lossy()?.into_owned())
}

fn push_path(path: &mut String, part: &str)
{
    if part.starts_with('/') || has_drive(part, '/') || has_backslash_root(part) || path.is_empty() {
        part.clone_into(path);
    } else {
        let separator = if has_backslash_root(path) { '\\' } else { '/' };
        if !path.ends_with(separator) {
            path.push(separator);
        }
        path.push_str(part);
    }
}

fn has_backslash_root(path: &str) -> bool
{
    path.starts_with('\\') || has_drive(path, '\\')
}

/// Whether `path` starts with a drive letter, a colon and `separator`, as in `C:/` or `C:\`.
fn has_drive(path: &str, separator: char) -> bool
{
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == separator as u8
}

impl BinaryImage
{
    /// The image's line index, built on first use from every compilation unit that can be
    /// parsed. The result is cached, including a failure to load the DWARF sections (see the
    /// module documentation).
    pub(crate) fn line_index(&self) -> Result<&LineIndex, SourceLookupError>
    {
        self.line_index_cell()
            .get_or_init(|| {
                let dwarf = self
                    .dwarf()
                    .map_err(|error| SourceLookupError::DebugInfo(error.to_string()))?;
                let index = LineIndex::build(dwarf, self.function_index()?.fragments());
                warn_skipped(self, "line", index.skipped());
                Ok(index)
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Debug info that could not be parsed and was left out of this image's line and
    /// function indexes, sorted and distinct; empty when everything was read. Builds both
    /// indexes if needed.
    ///
    /// A lookup that fails with [`SourceLookupError::UnknownFile`] or
    /// [`SourceLookupError::UnknownFunction`] while this is not empty may have missed a file
    /// or function in the skipped debug info.
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::DebugInfo`] when the image's DWARF sections cannot be loaded.
    pub fn skipped_debug_info(&self) -> Result<Vec<SkippedDebugInfo>, SourceLookupError>
    {
        Ok(merge_skipped(self.line_index()?.skipped(), self.function_index()?.skipped()))
    }

    /// Every statement address of `file:line`, across all compilation units.
    ///
    /// `file` matches a DWARF source path on path-component boundaries: `symbols.rs`,
    /// `fixtures/symbols.rs` and `src/main.rs` match any path ending in those components,
    /// and `os.rs` does not match `macos.rs`. An absolute `file` must equal the full path.
    /// If more than one distinct path matches, the lookup fails with
    /// [`SourceLookupError::AmbiguousFile`].
    ///
    /// When `line` has no code, the nearest following line of the same file that has code
    /// is used; [`LineAddresses::line`] reports the line actually used. Each contiguous run
    /// of line-table rows for that line (an inlined copy, a generic instantiation, or a
    /// line that is re-entered after other lines) contributes its first statement
    /// address.
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::InvalidLine`] for line 0, [`SourceLookupError::UnknownFile`]
    /// or [`SourceLookupError::AmbiguousFile`] when `file` matches no or several paths,
    /// [`SourceLookupError::NoCodeAtOrAfterLine`] when `line` is past the file's last line
    /// with code, and [`SourceLookupError::DebugInfo`] when the DWARF sections cannot be
    /// loaded. Units that cannot be parsed are skipped rather than failing the lookup (see
    /// [`BinaryImage::skipped_debug_info`]).
    pub fn line_addresses(&self, file: &str, line: u32) -> Result<LineAddresses, SourceLookupError>
    {
        if line == 0 {
            return Err(SourceLookupError::InvalidLine { line });
        }
        let index = self.line_index()?;
        let id = index.resolve_file(file)?;
        let path = &index.files[id as usize];
        let lines = &index.lines[id as usize];
        // The first line at or after `line` with an address mapped in this image (a line
        // whose rows all lie outside the image's segments has no code here).
        let found = lines.range(line..).find_map(|(&used, addresses)| {
            let mut addresses: Vec<Address> = addresses
                .iter()
                .filter_map(|&address| self.runtime_address(address))
                .collect();
            addresses.sort_unstable();
            addresses.dedup();
            (!addresses.is_empty()).then_some((used, addresses))
        });
        let Some((used, addresses)) = found else {
            return Err(SourceLookupError::NoCodeAtOrAfterLine {
                file: path.clone(),
                line,
                last_line: lines.keys().next_back().copied().unwrap_or(0),
            });
        };
        Ok(LineAddresses {
            file: path.clone(),
            line: used,
            addresses,
        })
    }

    /// The source line containing `address`, from the line-table row that covers it, and
    /// every address range of that line in the same line-table sequence and the same
    /// function fragment (one sequence can hold several functions, such as two
    /// instantiations of a generic function, or the cold parts of several split functions,
    /// that share line numbers; a split function's hot and cold parts are separate
    /// fragments).
    ///
    /// Returns `Ok(None)` when the address is outside the image, outside every line-table
    /// sequence, or in a row with no source line (line 0, compiler-generated code).
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::DebugInfo`] when the DWARF sections cannot be loaded.
    pub fn line_at_address(&self, address: Address) -> Result<Option<AddressLine>, SourceLookupError>
    {
        let Some(file_address) = self.file_address(address) else {
            return Ok(None);
        };
        let index = self.line_index()?;
        let Some((sequence, at)) = index.row_at(file_address) else {
            return Ok(None);
        };
        let row = sequence.rows[at];
        if row.line == 0 {
            return Ok(None);
        }
        let keys: Vec<(u32, u32)> = sequence.rows.iter().map(|row| (row.file, row.line)).collect();
        let ranges = self.runtime_ranges(line_ranges(
            sequence,
            &keys,
            (row.file, row.line),
            index.function_bounds(file_address),
        ));
        Ok(Some(AddressLine {
            file: index.files[row.file as usize].clone(),
            line: row.line,
            column: (row.column != 0).then_some(row.column),
            ranges,
        }))
    }

    /// The statement containing `address`, for choosing where a source-line step stops.
    ///
    /// Like [`line_at_address`](Self::line_at_address), except that only `is_stmt` rows
    /// start a line: a row without `is_stmt` (optimised code interleaving the
    /// instructions of several lines) belongs to the line of the closest `is_stmt` row
    /// before it in its sequence, so a step does not stop there. A sequence that starts
    /// with rows that are not statements keeps their own lines until its first statement.
    /// The reported `line` and `ranges` are that statement's; `column` is the statement
    /// row's.
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::DebugInfo`] when the DWARF sections cannot be loaded.
    pub fn statement_at_address(&self, address: Address) -> Result<Option<AddressLine>, SourceLookupError>
    {
        let Some(file_address) = self.file_address(address) else {
            return Ok(None);
        };
        let index = self.line_index()?;
        let Some((sequence, at)) = index.row_at(file_address) else {
            return Ok(None);
        };
        let Some((key, column)) = index.statement_at(sequence, at) else {
            return Ok(None);
        };
        let ranges = self.runtime_ranges(index.statement_ranges(sequence, at, file_address));
        Ok(Some(AddressLine {
            file: index.files[key.0 as usize].clone(),
            line: key.1,
            column: (column != 0).then_some(column),
            ranges,
        }))
    }

    /// File virtual ranges as runtime ranges; a range outside the image is dropped.
    fn runtime_ranges(&self, ranges: Vec<Range<u64>>) -> Vec<Range<Address>>
    {
        ranges
            .into_iter()
            .filter_map(|range| Some(self.runtime_address(range.start)?..self.runtime_address(range.end - 1)? + 1))
            .collect()
    }
}

/// Every `[start, end)` of `sequence` whose row has key `key` (by `keys`, one per row),
/// within `function`, adjacent ranges merged, sorted by start.
fn line_ranges(sequence: &LineSequence, keys: &[(u32, u32)], key: (u32, u32), function: Range<u64>) -> Vec<Range<u64>>
{
    let mut ranges: Vec<Range<u64>> = Vec::new();
    for (at, pair) in sequence.rows.windows(2).enumerate() {
        let (start, next) = (pair[0], pair[1]);
        if keys[at] != key || start.address >= next.address || !function.contains(&start.address) {
            continue;
        }
        let end = next.address.min(function.end);
        match ranges.last_mut() {
            Some(last) if last.end == start.address => last.end = end,
            _ => ranges.push(start.address..end),
        }
    }
    ranges
}

/// The skips of the line and function indexes together, sorted and distinct: a unit that
/// neither index could load is reported once.
pub(crate) fn merge_skipped(lines: &[SkippedDebugInfo], functions: &[SkippedDebugInfo]) -> Vec<SkippedDebugInfo>
{
    let mut skipped: Vec<SkippedDebugInfo> = lines.iter().chain(functions).cloned().collect();
    skipped.sort();
    skipped.dedup();
    skipped
}

/// Log every skipped part of the debug info of `image`'s `kind` index.
pub(crate) fn warn_skipped(image: &BinaryImage, kind: &str, skipped: &[SkippedDebugInfo])
{
    for skip in skipped {
        tracing::warn!(
            image = %image.path().display(),
            unit_offset = ?skip.unit_offset,
            reason = %skip.reason,
            "skipped unreadable debug info while building the {kind} index"
        );
    }
}

/// Synthetic DWARF for index tests, written with `gimli::write`.
#[cfg(test)]
pub(crate) mod synthetic
{
    use std::collections::HashMap;
    use std::sync::Arc;

    use gimli::write::{
        Address, AttributeValue, DwarfUnit, EndianVec, LineProgram, LineString, Range, RangeList, Sections, UnitEntryId,
    };
    use gimli::{
        DW_AT_comp_dir, DW_AT_entry_pc, DW_AT_linkage_name, DW_AT_name, DW_AT_ranges, DW_TAG_subprogram, DwAt, Encoding,
        EndianArcSlice, Format, LineEncoding, LittleEndian, RunTimeEndian, SectionId,
    };

    use super::OwnedDwarf;

    /// DWARF 4, 64-bit addresses: what rustc emits on macOS.
    pub(crate) const ENCODING: Encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };

    /// One compilation unit to write.
    pub(crate) struct TestUnit
    {
        /// `DW_AT_comp_dir`, also the line program's directory 0.
        pub(crate) comp_dir: &'static str,
        /// Line-table rows `(address, line)` of one sequence in file `file` (relative to
        /// `comp_dir`), ending at `end`.
        pub(crate) file: &'static str,
        pub(crate) rows: Vec<(u64, u64)>,
        pub(crate) end: u64,
        /// More sequences `(rows, end)` in the same file, written after the first.
        pub(crate) more_sequences: Vec<(Vec<(u64, u64)>, u64)>,
        /// Row addresses (of any sequence) written without `is_stmt`.
        pub(crate) non_statements: Vec<u64>,
        /// Subprograms of the unit.
        pub(crate) functions: Vec<TestFunction>,
        /// Extra attributes `(function name, attribute, value)` set on a subprogram after
        /// its usual ones (replacing an attribute it already has).
        pub(crate) attributes: Vec<(&'static str, DwAt, AttributeValue)>,
    }

    /// One subprogram: `(name, [start, end) ranges in listed order, DW_AT_entry_pc)`. The
    /// name is written as both `DW_AT_name` and `DW_AT_linkage_name`.
    pub(crate) type TestFunction = (&'static str, Vec<(u64, u64)>, Option<u64>);

    impl TestUnit
    {
        pub(crate) fn new(comp_dir: &'static str, file: &'static str) -> Self
        {
            Self {
                comp_dir,
                file,
                rows: Vec::new(),
                end: 0,
                more_sequences: Vec::new(),
                non_statements: Vec::new(),
                functions: Vec::new(),
                attributes: Vec::new(),
            }
        }
    }

    /// Written section bytes, by section.
    pub(crate) struct TestDwarf
    {
        pub(crate) sections: HashMap<SectionId, Vec<u8>>,
    }

    /// Write `units` (in order) to DWARF sections.
    pub(crate) fn write(units: &[TestUnit]) -> TestDwarf
    {
        let mut dwarf = gimli::write::Dwarf::new();
        for spec in units {
            let mut line_program = LineProgram::new(
                ENCODING,
                LineEncoding::default(),
                LineString::String(spec.comp_dir.into()),
                None,
                LineString::String(spec.file.into()),
                None,
            );
            let directory = line_program.default_directory();
            let file = line_program.add_file(LineString::String(spec.file.into()), directory, None);
            let first = (spec.rows.clone(), spec.end);
            for (rows, end) in std::iter::once(&first).chain(&spec.more_sequences) {
                let Some(&(base, _)) = rows.first() else {
                    continue;
                };
                line_program.begin_sequence(Some(Address::Constant(base)));
                for &(address, line) in rows {
                    let row = line_program.row();
                    row.address_offset = address - base;
                    row.file = file;
                    row.line = line;
                    row.is_statement = !spec.non_statements.contains(&address);
                    line_program.generate_row();
                }
                line_program.end_sequence(end - base);
            }
            let mut unit = DwarfUnit::new(ENCODING);
            unit.unit.line_program = line_program;
            let root = unit.unit.root();
            let entry = unit.unit.get_mut(root);
            entry.set(DW_AT_name, AttributeValue::String(spec.file.into()));
            entry.set(DW_AT_comp_dir, AttributeValue::String(spec.comp_dir.into()));
            for (name, ranges, entry_pc) in &spec.functions {
                let list = RangeList(
                    ranges
                        .iter()
                        .map(|&(begin, end)| Range::StartEnd {
                            begin: Address::Constant(begin),
                            end: Address::Constant(end),
                        })
                        .collect(),
                );
                let list = unit.unit.ranges.add(list);
                let id: UnitEntryId = unit.unit.add(root, DW_TAG_subprogram);
                let die = unit.unit.get_mut(id);
                die.set(DW_AT_name, AttributeValue::String((*name).into()));
                // Every rustc subprogram has a linkage name; the index resolves names
                // through it.
                die.set(DW_AT_linkage_name, AttributeValue::String((*name).into()));
                die.set(DW_AT_ranges, AttributeValue::RangeListRef(list));
                if let Some(entry_pc) = entry_pc {
                    die.set(DW_AT_entry_pc, AttributeValue::Address(Address::Constant(*entry_pc)));
                }
                for (_, attribute, value) in spec.attributes.iter().filter(|(function, ..)| function == name) {
                    die.set(*attribute, value.clone());
                }
            }
            dwarf.units.add(unit.unit);
        }
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        dwarf.write(&mut sections).expect("write DWARF");
        let mut bytes = HashMap::new();
        sections
            .for_each(|id, section| {
                bytes.insert(id, section.slice().to_vec());
                Ok::<_, gimli::Error>(())
            })
            .expect("collect sections");
        TestDwarf { sections: bytes }
    }

    /// Two units, `src/bad.rs` (function `bad_fn`, lines 3-4) and `src/good.rs` (function
    /// `good_fn` at `0x2000`, lines 7-8), where the first unit's header parses but its root
    /// entry does not. Returns the DWARF and the bad unit's `.debug_info` offset.
    pub(crate) fn corrupt_then_good() -> (OwnedDwarf, u64)
    {
        let mut bad = TestUnit::new("/w", "src/bad.rs");
        bad.rows = vec![(0x1000, 3), (0x1004, 4)];
        bad.end = 0x1008;
        bad.functions = vec![("bad_fn", vec![(0x1000, 0x1008)], None)];
        let mut good = TestUnit::new("/w", "src/good.rs");
        good.rows = vec![(0x2000, 7), (0x2008, 8)];
        good.end = 0x2010;
        good.functions = vec![("good_fn", vec![(0x2000, 0x2010)], None)];
        let mut written = write(&[bad, good]);
        // DWARF 4 unit header: 4-byte length, 2-byte version, 4-byte abbreviation offset,
        // 1-byte address size. The root entry's abbreviation code follows; 0x7f is not in
        // the table, so the header parses and the unit does not.
        written.corrupt_info(11, 0x7f);
        let dwarf = written.load();
        let mut units = dwarf.units();
        let header = units.next().expect("first header").expect("first unit");
        assert_eq!(header.offset().as_debug_info_offset().map(|offset| offset.0), Some(0));
        assert!(dwarf.unit(header).is_err(), "the first unit must fail to parse");
        assert!(
            units.next().expect("second header").is_some(),
            "the second unit must be found"
        );
        (dwarf, 0)
    }

    impl TestDwarf
    {
        /// Load the sections the way `BinaryImage` does.
        pub(crate) fn load(&self) -> OwnedDwarf
        {
            gimli::Dwarf::load(|id: SectionId| {
                let data = self.sections.get(&id).map_or(&[][..], Vec::as_slice);
                Ok::<_, gimli::Error>(EndianArcSlice::new(Arc::from(data), RunTimeEndian::Little))
            })
            .expect("load DWARF")
        }

        /// Overwrite one byte of `.debug_info`.
        pub(crate) fn corrupt_info(&mut self, offset: usize, byte: u8)
        {
            self.corrupt(SectionId::DebugInfo, offset, byte);
        }

        /// Overwrite one byte of section `id`.
        pub(crate) fn corrupt(&mut self, id: SectionId, offset: usize, byte: u8)
        {
            self.sections.get_mut(&id).expect("written section")[offset] = byte;
        }

        /// Offsets of the units of section `id` (`.debug_info` or `.debug_line`, both
        /// starting each unit with a 32-bit length), in order.
        pub(crate) fn unit_starts(&self, id: SectionId) -> Vec<usize>
        {
            let bytes = self.sections.get(&id).map_or(&[][..], Vec::as_slice);
            let mut starts = Vec::new();
            let mut next = 0;
            while next + 4 <= bytes.len() {
                starts.push(next);
                let length = u32::from_le_bytes(bytes[next..next + 4].try_into().expect("4 bytes"));
                next += 4 + length as usize;
            }
            starts
        }

        /// Make the line-program header of unit `unit` unparseable (an unknown version),
        /// leaving its `.debug_info` intact.
        pub(crate) fn corrupt_line_header(&mut self, unit: usize)
        {
            let start = self.unit_starts(SectionId::DebugLine)[unit];
            // The 2-byte version follows the 4-byte length.
            self.corrupt(SectionId::DebugLine, start + 4, 0x99);
            self.corrupt(SectionId::DebugLine, start + 5, 0);
        }

        /// Make the `.debug_info` unit header of unit `unit` unparseable (an unknown
        /// version), so neither it nor any later unit can be located.
        pub(crate) fn corrupt_unit_header(&mut self, unit: usize)
        {
            let start = self.unit_starts(SectionId::DebugInfo)[unit];
            self.corrupt(SectionId::DebugInfo, start + 4, 0x99);
            self.corrupt(SectionId::DebugInfo, start + 5, 0);
        }

        /// `.debug_info` offset of the subprogram DIE named `name`.
        pub(crate) fn subprogram_offset(&self, name: &str) -> usize
        {
            let dwarf = self.load();
            let mut units = dwarf.units();
            while let Some(header) = units.next().expect("unit header") {
                let base = header.offset().as_debug_info_offset().expect("info unit").0;
                let unit = dwarf.unit(header).expect("unit");
                let mut entries = unit.entries();
                while let Some((_, entry)) = entries.next_dfs().expect("entry") {
                    if entry.tag() != DW_TAG_subprogram {
                        continue;
                    }
                    let Some(value) = entry.attr_value(DW_AT_name).expect("name") else {
                        continue;
                    };
                    if dwarf.attr_string(&unit, value).ok().as_deref() == Some(name.as_bytes()) {
                        return base + entry.offset().0;
                    }
                }
            }
            panic!("no subprogram {name}");
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::synthetic::{TestUnit, write};
    use super::*;

    #[test]
    fn push_path_keeps_rooted_parts_and_joins_relative_ones()
    {
        let joined = |base: &str, part: &str| {
            let mut path = base.to_owned();
            push_path(&mut path, part);
            path
        };
        assert_eq!(joined("/build", "C:/src/main.rs"), "C:/src/main.rs");
        assert_eq!(joined("/build", "C:\\src\\main.rs"), "C:\\src\\main.rs");
        assert_eq!(joined("/build", "\\\\host\\share\\a.rs"), "\\\\host\\share\\a.rs");
        assert_eq!(joined("/build", "1:/src/main.rs"), "/build/1:/src/main.rs", "not a drive");
        assert_eq!(joined("C:\\build", "src\\a.rs"), "C:\\build\\src\\a.rs");
    }

    /// AC1 (issue #88, L4): a unit whose body does not parse is skipped; the good unit is
    /// still indexed and the skip is recorded.
    #[test]
    fn a_corrupt_unit_does_not_hide_the_good_one()
    {
        let (dwarf, bad_offset) = super::synthetic::corrupt_then_good();
        let lines = LineIndex::build(&dwarf, &[]);
        let file = lines.resolve_file("good.rs").expect("good.rs");
        assert_eq!(lines.files[file as usize], "/w/src/good.rs");
        assert_eq!(lines.lines[file as usize].get(&8), Some(&vec![0x2008]));
        assert!(matches!(
            lines.resolve_file("bad.rs"),
            Err(SourceLookupError::UnknownFile { .. })
        ));
        assert_eq!(lines.skipped().len(), 1, "{:?}", lines.skipped());
        assert_eq!(lines.skipped()[0].unit_offset, Some(bad_offset));
    }

    /// Issue #93: when the rows of a line program stop parsing part-way, the sequences
    /// completed before the error are kept and a `line program` skip is recorded.
    #[test]
    fn a_line_program_cut_short_keeps_its_completed_sequences()
    {
        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.rows = vec![(0x1000, 3), (0x1008, 4)];
        unit.end = 0x1010;
        unit.more_sequences = vec![(vec![(0x2000, 7), (0x2008, 8)], 0x2010)];
        let mut written = write(&[unit]);
        // Shorten the program by one byte: the final `DW_LNE_end_sequence` (opcode 0,
        // length 1, then the sub-opcode) loses its sub-opcode and no longer parses.
        let start = written.unit_starts(gimli::SectionId::DebugLine)[0];
        let line = written.sections.get_mut(&gimli::SectionId::DebugLine).expect(".debug_line");
        assert_eq!(&line[line.len() - 3..], &[0, 1, 1], "the program ends with end_sequence");
        let length = u32::from_le_bytes(line[start..start + 4].try_into().expect("4 bytes")) - 1;
        line[start..start + 4].copy_from_slice(&length.to_le_bytes());
        let dwarf = written.load();
        let lines = LineIndex::build(&dwarf, &[]);
        assert_eq!(
            lines
                .sequences
                .iter()
                .map(|sequence| sequence.start..sequence.end)
                .collect::<Vec<_>>(),
            vec![0x1000..0x1010]
        );
        let file = lines.resolve_file("lib.rs").expect("lib.rs");
        assert_eq!(lines.lines[file as usize].get(&4), Some(&vec![0x1008]));
        assert_eq!(lines.lines[file as usize].get(&8), None);
        assert_eq!(lines.skipped().len(), 1, "{:?}", lines.skipped());
        assert_eq!(lines.skipped()[0].unit_offset, Some(0));
        assert!(
            lines.skipped()[0].reason.starts_with("line program: "),
            "{:?}",
            lines.skipped()
        );
    }

    /// Issue #93: the skips of both indexes are merged sorted, and a skip both report is
    /// kept once.
    #[test]
    fn skips_of_both_indexes_are_merged_and_deduplicated()
    {
        let skip = |unit_offset, reason: &str| SkippedDebugInfo {
            unit_offset,
            reason: reason.to_owned(),
        };
        let lines = [skip(Some(0x40), "unit: bad"), skip(Some(0x10), "line program: eof")];
        let functions = [
            skip(Some(0x40), "unit: bad"),
            skip(None, "unit header: bad"),
            skip(Some(0x10), "DIE tree: eof"),
        ];
        assert_eq!(
            merge_skipped(&lines, &functions),
            vec![
                skip(None, "unit header: bad"),
                skip(Some(0x10), "DIE tree: eof"),
                skip(Some(0x10), "line program: eof"),
                skip(Some(0x40), "unit: bad"),
            ]
        );
    }

    /// AC2 (issue #88, L5): two spellings of one path are one file, found by an absolute
    /// and by a suffix query, with the addresses of both units.
    #[test]
    fn two_spellings_of_one_file_are_one_file()
    {
        let mut first = TestUnit::new("/w/.", "src/lib.rs");
        first.rows = vec![(0x1000, 5)];
        first.end = 0x1004;
        let mut second = TestUnit::new("/w", "src//lib.rs");
        second.rows = vec![(0x2000, 5)];
        second.end = 0x2004;
        let dwarf = write(&[first, second]).load();
        let lines = LineIndex::build(&dwarf, &[]);
        for query in ["/w/src/lib.rs", "/w/./src/lib.rs", "src/lib.rs", "lib.rs"] {
            let file = lines.resolve_file(query).unwrap_or_else(|error| panic!("{query}: {error}"));
            assert_eq!(lines.files[file as usize], "/w/src/lib.rs", "{query}");
            assert_eq!(lines.lines[file as usize].get(&5), Some(&vec![0x1000, 0x2000]), "{query}");
        }
    }

    /// `normalize_path` drops empty and `.` components and keeps `..`.
    #[test]
    fn paths_normalize_without_resolving_parent_components()
    {
        assert_eq!(normalize_path("/w/./src//lib.rs"), "/w/src/lib.rs");
        assert_eq!(normalize_path("/w/a/../lib.rs"), "/w/a/../lib.rs");
        assert_eq!(normalize_path("./src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path(""), "");
    }

    fn index(paths: &[&str]) -> LineIndex
    {
        LineIndex {
            files: paths.iter().map(|path| (*path).to_owned()).collect(),
            sequences: Vec::new(),
            lines: paths.iter().map(|_| BTreeMap::from([(1, vec![0x1000])])).collect(),
            boundaries: Vec::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn suffix_matching_respects_component_boundaries()
    {
        let index = index(&["/w/src/macos.rs", "/w/src/os.rs", "/w/a/src/main.rs", "/w/b/src/main.rs"]);
        assert_eq!(index.resolve_file("os.rs"), Ok(1));
        assert_eq!(index.resolve_file("macos.rs"), Ok(0));
        assert_eq!(index.resolve_file("src/os.rs"), Ok(1));
        assert_eq!(index.resolve_file("a/src/main.rs"), Ok(2));
        assert_eq!(index.resolve_file("/w/b/src/main.rs"), Ok(3));
        assert_eq!(
            index.resolve_file("s.rs"),
            Err(SourceLookupError::UnknownFile { query: "s.rs".into() })
        );
        assert_eq!(
            index.resolve_file("/src/os.rs"),
            Err(SourceLookupError::UnknownFile {
                query: "/src/os.rs".into()
            })
        );
        assert_eq!(
            index.resolve_file("src/main.rs"),
            Err(SourceLookupError::AmbiguousFile {
                query: "src/main.rs".into(),
                candidates: vec!["/w/a/src/main.rs".into(), "/w/b/src/main.rs".into()],
            })
        );
    }

    #[test]
    fn canonical_address_is_the_first_statement_row_of_each_run()
    {
        let row = |address, line, is_stmt| LineRow {
            address,
            file: 0,
            line,
            column: 1,
            is_stmt,
            prologue_end: false,
            end_sequence: false,
        };
        let mut end = row(0x40, 0, false);
        end.end_sequence = true;
        let sequence = LineSequence {
            start: 0x0,
            end: 0x40,
            rows: vec![
                row(0x0, 5, false),
                row(0x4, 5, true),
                row(0x8, 5, true),
                row(0x10, 6, true),
                row(0x20, 0, true),
                row(0x30, 5, true),
                end,
            ],
        };
        let mut lines = vec![BTreeMap::new()];
        index_statements(&mut lines, &sequence, &[]);
        assert_eq!(lines[0].get(&5), Some(&vec![0x4, 0x30]));
        assert_eq!(lines[0].get(&6), Some(&vec![0x10]));
        assert_eq!(lines[0].get(&0), None);

        // A fragment boundary inside a run (two copies of a one-line function laid out back
        // to back in one sequence) starts a new run.
        let mut lines = vec![BTreeMap::new()];
        index_statements(&mut lines, &sequence, &[0x0, 0x8]);
        assert_eq!(lines[0].get(&5), Some(&vec![0x4, 0x8, 0x30]));
    }

    /// Issue #120 (5): rows without `is_stmt` do not start a line for a step. Line 5 at
    /// 0x1000 is interleaved with a non-statement row of line 7 (0x1004); line 6 at 0x1008
    /// is followed by a non-statement row of line 5 (0x100c). A step from line 5 runs
    /// through 0x1004 and stops at 0x1008, and 0x100c belongs to line 6's statement.
    #[test]
    fn a_line_step_ignores_rows_that_are_not_statements()
    {
        use super::super::functions::FunctionIndex;

        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.rows = vec![(0x1000, 5), (0x1004, 7), (0x1008, 6), (0x100c, 5), (0x1010, 8)];
        unit.end = 0x1014;
        unit.non_statements = vec![0x1004, 0x100c];
        unit.functions = vec![("f", vec![(0x1000, 0x1014)], None)];
        let dwarf = write(&[unit]).load();
        let functions = FunctionIndex::build(&dwarf);
        let index = LineIndex::build(&dwarf, functions.fragments());
        let (sequence, at) = index.row_at(0x1000).expect("row");
        assert!(!sequence.rows[1].is_stmt, "the writer emits non-statement rows");

        // Every row: the display line splits line 5 at 0x1004 ...
        let keys: Vec<(u32, u32)> = sequence.rows.iter().map(|row| (row.file, row.line)).collect();
        assert_eq!(
            line_ranges(sequence, &keys, keys[at], index.function_bounds(0x1000)),
            vec![0x1000..0x1004, 0x100c..0x1010]
        );
        // ... statements only: line 5 runs on through the line-7 row to line 6.
        assert_eq!(index.statement_ranges(sequence, at, 0x1000), vec![0x1000..0x1008]);
        let (_, at) = index.row_at(0x100c).expect("row");
        assert_eq!(index.statement_at(sequence, at).map(|(key, _)| key.1), Some(6));
        assert_eq!(index.statement_ranges(sequence, at, 0x100c), vec![0x1008..0x1010]);
        let (_, at) = index.row_at(0x1004).expect("row");
        assert_eq!(index.statement_at(sequence, at).map(|(key, _)| key.1), Some(5));

        // A statement does not carry into the next function's leading non-statement rows,
        // even with no row exactly at the function's start.
        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.rows = vec![(0x2000, 3), (0x200c, 9), (0x2010, 10)];
        unit.end = 0x2014;
        unit.non_statements = vec![0x200c];
        unit.functions = vec![("f", vec![(0x2000, 0x2008)], None), ("g", vec![(0x2008, 0x2014)], None)];
        let dwarf = write(&[unit]).load();
        let functions = FunctionIndex::build(&dwarf);
        let index = LineIndex::build(&dwarf, functions.fragments());
        let (sequence, at) = index.row_at(0x200c).expect("row");
        assert_eq!(index.statement_at(sequence, at).map(|(key, _)| key.1), Some(9));
    }

    #[test]
    fn function_bounds_split_a_sequence_at_fragment_starts()
    {
        let index = LineIndex::with_fragments(&[0x100..0x120, 0x120..0x140]);
        assert_eq!(index.function_bounds(0x110), 0x100..0x120);
        assert_eq!(index.function_bounds(0x120), 0x120..0x140);
        assert_eq!(index.function_bounds(0x150), 0x140..u64::MAX);
        assert_eq!(index.function_bounds(0x10), 0..0x100);
    }

    /// Issue #92: two split functions whose cold parts are laid out back to back in one
    /// sequence (`G.cold`, then `F.cold`) and share a file and line, with hot parts (and
    /// entries) elsewhere. `F` also has a `DW_AT_entry_pc` inside its hot part.
    fn split_functions() -> LineIndex
    {
        use super::super::functions::FunctionIndex;

        let mut unit = TestUnit::new("/w", "src/lib.rs");
        unit.rows = vec![(0x1000, 9), (0x1010, 9), (0x1020, 3), (0x1030, 3), (0x1040, 5)];
        unit.end = 0x1050;
        unit.functions = vec![
            ("g", vec![(0x1030, 0x1040), (0x1000, 0x1010)], None),
            ("f", vec![(0x1040, 0x1050), (0x1010, 0x1020)], Some(0x1048)),
            ("h", vec![(0x1020, 0x1030)], None),
        ];
        let dwarf = write(&[unit]).load();
        let functions = FunctionIndex::build(&dwarf);
        assert!(functions.skipped().is_empty(), "{:?}", functions.skipped());
        LineIndex::build(&dwarf, functions.fragments())
    }

    /// AC1 (issue #92): adjacent cold fragments that share a file and line each get an
    /// address from the line map.
    #[test]
    fn adjacent_cold_fragments_each_get_an_address()
    {
        let lines = split_functions();
        let file = lines.resolve_file("lib.rs").expect("lib.rs");
        assert_eq!(lines.lines[file as usize].get(&9), Some(&vec![0x1000, 0x1010]));
        // `h` follows `F.cold` with another line, and `g`'s hot part repeats line 3.
        assert_eq!(lines.lines[file as usize].get(&3), Some(&vec![0x1020, 0x1030]));
    }

    /// AC2 (issue #92): the bounds of an address in a cold fragment are that fragment's,
    /// and the bytes of a hot part before its `DW_AT_entry_pc` are bounded by the part.
    #[test]
    fn function_bounds_of_a_cold_fragment_is_the_fragment()
    {
        let lines = split_functions();
        assert_eq!(lines.function_bounds(0x1004), 0x1000..0x1010);
        assert_eq!(lines.function_bounds(0x1018), 0x1010..0x1020);
        assert_eq!(lines.function_bounds(0x1044), 0x1040..0x1050);
        assert_eq!(lines.function_bounds(0x104c), 0x1040..0x1050);
    }
}
