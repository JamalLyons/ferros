//! Source line tables: `file:line` to addresses, and address to line and ranges.
//!
//! The index is built once per [`BinaryImage`], on the first lookup, from the line program
//! of every compilation unit. File paths are rendered the way `addr2line` renders them
//! (compilation directory, then include directory, then file name, where an absolute
//! component replaces what came before), so the paths here match
//! [`BinaryImage::symbolicate`].
//!
//! All public lookups take and return runtime [`Address`]es; the index itself stores file
//! virtual addresses and translates through the image mapping.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use gimli::{AttributeValue, ColumnType, FileEntry, LineProgramHeader, Reader as _, Unit};

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

    /// The image's DWARF line tables or debugging information entries could not be loaded
    /// or parsed.
    #[error("failed to read debug info: {0}")]
    DebugInfo(String),
}

/// The statement addresses of one source line, as returned by [`BinaryImage::line_addresses`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAddresses
{
    /// Full path of the matched source file, as recorded in DWARF.
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
    /// Full path of the source file, as recorded in DWARF.
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
/// Rows that share an address are collapsed into the last one, as `addr2line` does; a
/// `prologue_end` flag on a collapsed row is kept.
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
    /// Distinct full file paths that have at least one row with a line.
    pub(crate) files: Vec<String>,
    /// Every non-empty sequence, sorted by start address.
    pub(crate) sequences: Vec<LineSequence>,
    /// Per file (same index as `files`): line -> canonical statement file addresses.
    lines: Vec<BTreeMap<u32, Vec<u64>>>,
}

impl LineIndex
{
    pub(crate) fn build(dwarf: &OwnedDwarf) -> Result<Self, SourceLookupError>
    {
        let mut index = Self::default();
        let mut paths = HashMap::<String, u32>::new();
        let mut units = dwarf.units();
        while let Some(header) = units.next().map_err(dwarf_error)? {
            let unit = dwarf.unit(header).map_err(dwarf_error)?;
            let Some(program) = unit.line_program.clone() else {
                continue;
            };
            // DWARF file index -> index into `files`, for this unit.
            let mut unit_files = HashMap::<u64, u32>::new();
            let mut rows = program.rows();
            let mut current = Vec::<LineRow>::new();
            while let Some((header, row)) = rows.next_row().map_err(dwarf_error)? {
                let file = match unit_files.get(&row.file_index()) {
                    Some(&file) => file,
                    None => {
                        let path = row
                            .file(header)
                            .map(|entry| render_file(dwarf, &unit, entry, header))
                            .transpose()?
                            .unwrap_or_default();
                        let next = u32::try_from(paths.len()).unwrap_or(u32::MAX);
                        let file = *paths.entry(path).or_insert(next);
                        unit_files.insert(row.file_index(), file);
                        file
                    }
                };
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
                        index.sequences.push(LineSequence {
                            start: rows[0].address,
                            end: line.address,
                            rows,
                        });
                    }
                }
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
            index_statements(&mut index.lines, sequence);
        }
        Ok(index)
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

/// Record the canonical statement address of every run of rows for one line.
///
/// A run is a maximal stretch of consecutive rows with the same file and line; its
/// canonical address is its first `is_stmt` row. Rows with line 0 end a run.
fn index_statements(lines: &mut [BTreeMap<u32, Vec<u64>>], sequence: &LineSequence)
{
    let mut run: Option<(u32, u32)> = None;
    let mut recorded = false;
    for row in sequence.rows.iter().filter(|row| !row.end_sequence) {
        let key = (row.file, row.line);
        if run != Some(key) {
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

/// Render a line-program file entry the way `addr2line` does.
fn render_file(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    file: &FileEntry<OwnedReader>,
    header: &LineProgramHeader<OwnedReader>,
) -> Result<String, SourceLookupError>
{
    let mut path = match &unit.comp_dir {
        Some(dir) => dir.to_string_lossy().map_err(dwarf_error)?.into_owned(),
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

fn attr_string(
    dwarf: &OwnedDwarf,
    unit: &Unit<OwnedReader>,
    value: AttributeValue<OwnedReader>,
) -> Result<String, SourceLookupError>
{
    let string = dwarf.attr_string(unit, value).map_err(dwarf_error)?;
    Ok(string.to_string_lossy().map_err(dwarf_error)?.into_owned())
}

fn push_path(path: &mut String, part: &str)
{
    if part.starts_with('/') || path.is_empty() {
        part.clone_into(path);
    } else {
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(part);
    }
}

fn dwarf_error(error: gimli::Error) -> SourceLookupError
{
    SourceLookupError::DebugInfo(error.to_string())
}

impl BinaryImage
{
    /// The image's line index, built from every compilation unit on first use.
    pub(crate) fn line_index(&self) -> Result<&LineIndex, SourceLookupError>
    {
        self.line_index_cell().get_or_try_init(|| {
            let dwarf = self
                .dwarf()
                .map_err(|error| SourceLookupError::DebugInfo(error.to_string()))?;
            LineIndex::build(dwarf)
        })
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
    /// with code, and [`SourceLookupError::DebugInfo`] when the line tables cannot be read.
    pub fn line_addresses(&self, file: &str, line: u32) -> Result<LineAddresses, SourceLookupError>
    {
        if line == 0 {
            return Err(SourceLookupError::InvalidLine { line });
        }
        let index = self.line_index()?;
        let id = index.resolve_file(file)?;
        let path = &index.files[id as usize];
        let lines = &index.lines[id as usize];
        let Some((&used, addresses)) = lines.range(line..).next() else {
            return Err(SourceLookupError::NoCodeAtOrAfterLine {
                file: path.clone(),
                line,
                last_line: lines.keys().next_back().copied().unwrap_or(0),
            });
        };
        let mut addresses: Vec<Address> = addresses
            .iter()
            .filter_map(|&address| self.runtime_address(address))
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        Ok(LineAddresses {
            file: path.clone(),
            line: used,
            addresses,
        })
    }

    /// The source line containing `address`, from the line-table row that covers it, and
    /// every address range of that line in the same line-table sequence.
    ///
    /// Returns `Ok(None)` when the address is outside the image, outside every line-table
    /// sequence, or in a row with no source line (line 0, compiler-generated code).
    ///
    /// # Errors
    ///
    /// [`SourceLookupError::DebugInfo`] when the line tables cannot be read.
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
        let mut ranges: Vec<Range<u64>> = Vec::new();
        for pair in sequence.rows.windows(2) {
            let (start, next) = (pair[0], pair[1]);
            if start.file != row.file || start.line != row.line || start.address >= next.address {
                continue;
            }
            match ranges.last_mut() {
                Some(last) if last.end == start.address => last.end = next.address,
                _ => ranges.push(start.address..next.address),
            }
        }
        let ranges = ranges
            .into_iter()
            .filter_map(|range| Some(self.runtime_address(range.start)?..self.runtime_address(range.end - 1)? + 1))
            .collect();
        Ok(Some(AddressLine {
            file: index.files[row.file as usize].clone(),
            line: row.line,
            column: (row.column != 0).then_some(row.column),
            ranges,
        }))
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn index(paths: &[&str]) -> LineIndex
    {
        LineIndex {
            files: paths.iter().map(|path| (*path).to_owned()).collect(),
            sequences: Vec::new(),
            lines: paths.iter().map(|_| BTreeMap::from([(1, vec![0x1000])])).collect(),
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
        index_statements(&mut lines, &sequence);
        assert_eq!(lines[0].get(&5), Some(&vec![0x4, 0x30]));
        assert_eq!(lines[0].get(&6), Some(&vec![0x10]));
        assert_eq!(lines[0].get(&0), None);
    }
}
