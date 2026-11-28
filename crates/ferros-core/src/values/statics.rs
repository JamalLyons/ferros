//! Statics and globals: variables with a fixed address in their image.
//!
//! A static is a named `DW_TAG_variable` whose `DW_AT_location` is a single address
//! operation (`DW_OP_addr`, or `DW_OP_addrx` through `.debug_addr`), wherever it is
//! declared: Rust statics at module level and inside functions (rustc nests the latter in a
//! namespace named after the function), and C globals and function-level `static`s that
//! carry their own `DW_AT_name`. Its name is qualified by the namespaces, types and
//! functions that enclose it, joined with `::`, such as `my_crate::config::LIMIT`.
//!
//! Several statics can share a qualified name: two `static S` in separate blocks of one
//! function, or the per-call-site statics a logging macro expands to. Each is its own entry,
//! told apart by address.
//!
//! The index stores link-time (file) addresses. A value is read at the address the loaded
//! image maps that file address to, so it follows the image's load slide.
//!
//! Left out:
//!
//! - thread-locals (`thread_local!`, `#[thread_local]`, C `_Thread_local`): their location
//!   is a per-thread offset (`DW_OP_form_tls_address`), not an address;
//! - declarations (`DW_AT_declaration`), which describe a static defined elsewhere;
//! - definitions without a name of their own, named only through `DW_AT_specification`
//!   (C++ class statics and namespace members defined out of line);
//! - Rust vtables (`<T as Trait>::{vtable}`), which are compiler data, not program statics.
//!
//! The index is built once per image, on the first lookup, from every compilation unit
//! that can be parsed. Units or entries that cannot be read are recorded in
//! [`StaticIndex::skipped`] and the rest are still indexed.

use gimli::{AttributeValue, DebugInfoOffset, DebuggingInformationEntry, Operation, Reader as _, constants};

use super::location::{Unavailable, ValueLocation, VariableValue, value_size};
use super::scope::Variable;
use super::types::DieOffset;
use crate::error::Result as DebuggerResult;
use crate::symbols::lines::SkippedDebugInfo;
use crate::symbols::{BinaryImage, OwnedDwarf, OwnedReader};
use crate::types::Address;

type Unit = gimli::Unit<OwnedReader>;
type Entry<'abbrev, 'unit> = DebuggingInformationEntry<'abbrev, 'unit, OwnedReader>;

/// A static of a loaded image with its value, as `globals` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Static
{
    /// Link-time address from the debug information.
    pub file_address: u64,
    /// Where the loaded image maps `file_address`; `None` when no mapped segment holds it.
    pub address: Option<Address>,
    /// The static read from the loaded image, named by its qualified name (such as
    /// `my_crate::config::LIMIT`), for [`Session::format_variable`] and
    /// [`Session::format_path`].
    ///
    /// [`Session::format_variable`]: crate::session::Session::format_variable
    /// [`Session::format_path`]: crate::session::Session::format_path
    pub variable: Variable,
}

/// One indexed static.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaticEntry
{
    /// Qualified name.
    pub(crate) name: String,
    /// `.debug_info` offset of the variable entry.
    pub(crate) die: usize,
    /// Link-time address.
    pub(crate) file_address: u64,
}

/// The statics of one image, sorted by qualified name (see the module documentation).
#[derive(Debug, Default)]
pub(crate) struct StaticIndex
{
    entries: Vec<StaticEntry>,
    skipped: Vec<SkippedDebugInfo>,
}

impl StaticIndex
{
    /// Index the statics of every compilation unit of `dwarf` that can be parsed.
    pub(crate) fn build(dwarf: &OwnedDwarf) -> Self
    {
        let mut index = Self::default();
        let mut headers = dwarf.units();
        loop {
            let header = match headers.next() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                Err(error) => {
                    index.skipped.push(SkippedDebugInfo::new(None, "unit header", error));
                    break;
                }
            };
            let Some(start) = header.offset().as_debug_info_offset().map(|offset| offset.0) else {
                continue;
            };
            match dwarf.unit(header) {
                Ok(unit) => index.index_unit(dwarf, &unit, start),
                Err(error) => index.skipped.push(SkippedDebugInfo::new(Some(start as u64), "unit", error)),
            }
        }
        index
            .entries
            .sort_by(|a, b| a.name.cmp(&b.name).then(a.file_address.cmp(&b.file_address)));
        index
            .entries
            .dedup_by(|a, b| a.name == b.name && a.file_address == b.file_address);
        index
    }

    fn index_unit(&mut self, dwarf: &OwnedDwarf, unit: &Unit, start: usize)
    {
        let unit_offset = Some(start as u64);
        // The name each enclosing entry adds to a qualified name, by depth.
        let mut scopes = Vec::<Option<String>>::new();
        let mut depth = 0_isize;
        let mut entries = unit.entries();
        loop {
            let (delta, entry) = match entries.next_dfs() {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err(error) => {
                    self.skipped.push(SkippedDebugInfo::new(unit_offset, "DIE tree", error));
                    break;
                }
            };
            depth += delta;
            scopes.truncate(usize::try_from(depth).unwrap_or(0));
            let offset = start + entry.offset().0;
            let name = match own_name(dwarf, unit, entry) {
                Ok(name) => name,
                Err(error) => {
                    let context = format!("DIE at .debug_info+{offset:#x}");
                    self.skipped.push(SkippedDebugInfo::new(unit_offset, &context, error));
                    scopes.push(None);
                    continue;
                }
            };
            let tag = entry.tag();
            if tag == constants::DW_TAG_variable
                && let Some(name) = &name
                && !name.contains("{vtable}")
                && !matches!(
                    entry.attr_value(constants::DW_AT_declaration),
                    Ok(Some(AttributeValue::Flag(true)))
                )
                && let Some(file_address) = static_address(dwarf, unit, entry)
            {
                let mut qualified: String = scopes.iter().flatten().fold(String::new(), |mut path, scope| {
                    path.push_str(scope);
                    path.push_str("::");
                    path
                });
                qualified.push_str(name);
                self.entries.push(StaticEntry {
                    name: qualified,
                    die: offset,
                    file_address,
                });
            }
            let scope = matches!(
                tag,
                constants::DW_TAG_namespace
                    | constants::DW_TAG_structure_type
                    | constants::DW_TAG_class_type
                    | constants::DW_TAG_union_type
                    | constants::DW_TAG_enumeration_type
                    | constants::DW_TAG_subprogram
            )
            .then_some(name)
            .flatten();
            scopes.push(scope);
        }
    }

    /// Debug info left out of the index because it could not be parsed.
    pub(crate) fn skipped(&self) -> &[SkippedDebugInfo]
    {
        &self.skipped
    }

    /// Every static, sorted by qualified name.
    pub(crate) fn entries(&self) -> &[StaticEntry]
    {
        &self.entries
    }

    /// The statics `name` refers to: those whose qualified name is `name` (a leading `::`
    /// is ignored), else those whose qualified name ends with `::name`, so `LIMIT` and
    /// `config::LIMIT` both find `my_crate::config::LIMIT`. More than one result is an
    /// ambiguous name.
    pub(crate) fn lookup(&self, name: &str) -> Vec<&StaticEntry>
    {
        let name = name.trim();
        let name = name.strip_prefix("::").unwrap_or(name);
        if name.is_empty() {
            return Vec::new();
        }
        let exact: Vec<_> = self.entries.iter().filter(|entry| entry.name == name).collect();
        if !exact.is_empty() {
            return exact;
        }
        self.entries
            .iter()
            .filter(|entry| entry.name.strip_suffix(name).is_some_and(|prefix| prefix.ends_with("::")))
            .collect()
    }
}

/// Read `entry` from the loaded `image`. The value is a [`Variable`] named by the
/// qualified name, so `print` formats it, and evaluates paths from it, as it does a local.
pub(crate) fn read_static(
    image: &BinaryImage,
    entry: &StaticEntry,
    mut read_memory: impl FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
) -> Static
{
    let ty = image
        .types()
        .ok()
        .and_then(|types| types.type_of(DieOffset(entry.die as u64)).and_then(|ty| types.get(ty)));
    let address = image.runtime_address(entry.file_address);
    let value = match address {
        None => VariableValue::unavailable(Unavailable::UnmappedAddress(entry.file_address)),
        Some(address) => VariableValue {
            location: Some(ValueLocation::Memory(address)),
            bytes: value_size(ty.as_ref().and_then(|ty| ty.size)).and_then(|len| {
                let bytes = read_memory(address, len).map_err(|error| Unavailable::Memory(error.to_string()))?;
                if bytes.len() == len {
                    Ok(bytes)
                } else {
                    Err(Unavailable::Memory(format!(
                        "read {} of {len} bytes at {address}",
                        bytes.len()
                    )))
                }
            }),
        },
    };
    let variable = Variable {
        name: entry.name.clone(),
        type_name: ty.as_ref().and_then(|ty| ty.name.clone()),
        image_id: image.id(),
        die_offset: DebugInfoOffset(entry.die),
        type_offset: ty.as_ref().and_then(|ty| usize::try_from(ty.die.0).ok().map(DebugInfoOffset)),
        value,
    };
    Static {
        file_address: entry.file_address,
        address,
        variable,
    }
}

/// The link-time address of a location that is one address operation, and nothing else.
fn static_address(dwarf: &OwnedDwarf, unit: &Unit, entry: &Entry<'_, '_>) -> Option<u64>
{
    let AttributeValue::Exprloc(expression) = entry.attr_value(constants::DW_AT_location).ok()?? else {
        return None;
    };
    let mut operations = expression.operations(unit.encoding());
    let address = match operations.next().ok()?? {
        Operation::Address { address } => address,
        Operation::AddressIndex { index } => dwarf.address(unit, index).ok()?,
        _ => return None,
    };
    operations.next().ok()?.is_none().then_some(address)
}

/// The entry's own `DW_AT_name`.
fn own_name(dwarf: &OwnedDwarf, unit: &Unit, entry: &Entry<'_, '_>) -> gimli::Result<Option<String>>
{
    match entry.attr_value(constants::DW_AT_name)? {
        Some(value) => Ok(Some(dwarf.attr_string(unit, value)?.to_string_lossy()?.into_owned())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests
{
    use gimli::RunTimeEndian;
    use gimli::write::{Address as FileAddress, AttributeValue as WriteAttr, DwarfUnit, Expression, UnitEntryId};

    use super::*;
    use crate::values::location::tests::{encoding, synthetic_dwarf};

    fn named(dwarf: &mut DwarfUnit, parent: UnitEntryId, tag: gimli::DwTag, name: &str) -> UnitEntryId
    {
        let id = dwarf.unit.add(parent, tag);
        dwarf
            .unit
            .get_mut(id)
            .set(gimli::DW_AT_name, WriteAttr::String(name.as_bytes().to_vec()));
        id
    }

    fn located(dwarf: &mut DwarfUnit, parent: UnitEntryId, name: &str, location: Expression) -> UnitEntryId
    {
        let id = named(dwarf, parent, gimli::DW_TAG_variable, name);
        dwarf
            .unit
            .get_mut(id)
            .set(gimli::DW_AT_location, WriteAttr::Exprloc(location));
        id
    }

    fn address(address: u64) -> Expression
    {
        let mut expression = Expression::new();
        expression.op_addr(FileAddress::Constant(address));
        expression
    }

    #[test]
    fn indexes_addressed_variables_under_qualified_names()
    {
        let mut dwarf = DwarfUnit::new(encoding(4));
        let root = dwarf.unit.root();
        let krate = named(&mut dwarf, root, gimli::DW_TAG_namespace, "app");
        located(&mut dwarf, krate, "COUNTER", address(0x1000));
        let module = named(&mut dwarf, krate, gimli::DW_TAG_namespace, "config");
        located(&mut dwarf, module, "LIMIT", address(0x1008));
        let function = named(&mut dwarf, krate, gimli::DW_TAG_subprogram, "run");
        located(&mut dwarf, function, "calls", address(0x1010));
        // A local on the stack, a thread-local, a declaration and a vtable are not statics.
        let mut frame = Expression::new();
        frame.op_fbreg(-8);
        located(&mut dwarf, function, "local", frame);
        let mut tls = Expression::new();
        tls.op(gimli::DW_OP_const8u);
        tls.op(gimli::DW_OP_form_tls_address);
        located(&mut dwarf, krate, "PER_THREAD", tls);
        let declared = located(&mut dwarf, krate, "DECLARED", address(0x1018));
        dwarf
            .unit
            .get_mut(declared)
            .set(gimli::DW_AT_declaration, WriteAttr::Flag(true));
        located(&mut dwarf, krate, "<app::S as core::fmt::Debug>::{vtable}", address(0x1020));
        let mut offset = Expression::new();
        offset.op_addr(FileAddress::Constant(0x1028));
        offset.op_constu(8);
        offset.op(gimli::DW_OP_plus);
        located(&mut dwarf, krate, "COMPUTED", offset);

        let index = StaticIndex::build(&synthetic_dwarf(dwarf, RunTimeEndian::Little));
        let found: Vec<_> = index
            .entries()
            .iter()
            .map(|entry| (entry.name.as_str(), entry.file_address))
            .collect();
        assert_eq!(
            found,
            [
                ("app::COUNTER", 0x1000),
                ("app::config::LIMIT", 0x1008),
                ("app::run::calls", 0x1010)
            ]
        );
        assert!(index.skipped().is_empty());
    }

    #[test]
    fn reads_an_indexed_address_from_the_address_table()
    {
        let mut dwarf = DwarfUnit::new(encoding(5));
        let root = dwarf.unit.root();
        // DW_OP_addrx 1: the second address of `.debug_addr` (the unit's base is zero).
        located(&mut dwarf, root, "INDEXED", Expression::raw(vec![gimli::DW_OP_addrx.0, 1]));
        let mut dwarf = synthetic_dwarf(dwarf, RunTimeEndian::Little);
        let table: Vec<u8> = [0x1000_u64, 0x2468]
            .iter()
            .flat_map(|address| address.to_le_bytes())
            .collect();
        dwarf.debug_addr = gimli::DebugAddr::from(OwnedReader::new(table.into(), RunTimeEndian::Little));
        let index = StaticIndex::build(&dwarf);
        let found: Vec<_> = index
            .entries()
            .iter()
            .map(|entry| (entry.name.as_str(), entry.file_address))
            .collect();
        assert_eq!(found, [("INDEXED", 0x2468)]);
    }

    #[test]
    fn statics_sharing_a_name_stay_separate()
    {
        let mut dwarf = DwarfUnit::new(encoding(4));
        let root = dwarf.unit.root();
        let function = named(&mut dwarf, root, gimli::DW_TAG_subprogram, "blocks");
        for (block, address) in [(0x100, 0x1008), (0x200, 0x1000)] {
            let block_id = dwarf.unit.add(function, gimli::DW_TAG_lexical_block);
            dwarf
                .unit
                .get_mut(block_id)
                .set(gimli::DW_AT_low_pc, WriteAttr::Address(FileAddress::Constant(block)));
            located(&mut dwarf, block_id, "S", super::tests::address(address));
        }
        let index = StaticIndex::build(&synthetic_dwarf(dwarf, RunTimeEndian::Little));
        let found: Vec<_> = index
            .lookup("S")
            .into_iter()
            .map(|entry| (entry.name.as_str(), entry.file_address))
            .collect();
        assert_eq!(found, [("blocks::S", 0x1000), ("blocks::S", 0x1008)]);
    }

    #[test]
    fn looks_up_exact_names_before_suffixes_on_path_boundaries()
    {
        let mut dwarf = DwarfUnit::new(encoding(4));
        let root = dwarf.unit.root();
        let krate = named(&mut dwarf, root, gimli::DW_TAG_namespace, "app");
        located(&mut dwarf, krate, "LIMIT", address(0x1000));
        let module = named(&mut dwarf, krate, gimli::DW_TAG_namespace, "config");
        located(&mut dwarf, module, "LIMIT", address(0x1008));
        located(&mut dwarf, krate, "MAX_LIMIT", address(0x1010));
        let index = StaticIndex::build(&synthetic_dwarf(dwarf, RunTimeEndian::Little));
        let names = |query| {
            index
                .lookup(query)
                .into_iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("app::LIMIT"), ["app::LIMIT"]);
        assert_eq!(names("::app::LIMIT"), ["app::LIMIT"]);
        assert_eq!(names("config::LIMIT"), ["app::config::LIMIT"]);
        assert_eq!(names("LIMIT"), ["app::LIMIT", "app::config::LIMIT"]);
        assert_eq!(names("IMIT"), Vec::<&str>::new());
        assert_eq!(names(""), Vec::<&str>::new());
    }
}
