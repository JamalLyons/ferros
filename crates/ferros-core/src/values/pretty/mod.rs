//! Type-specific renderers for values whose logical contents differ from their fields.

mod dyn_trait;
mod enums;
mod maps;
mod shared;
mod strings;
mod vec;

use gimli::RunTimeEndian;

pub(super) use self::vec::layout as vec_layout;
use super::format::{ValueFormatter, ValueView, format_raw, unsigned};
use super::types::{BaseEncoding, Type, TypeId, TypeKind, TypeTable};
use crate::types::Address;

const BOX_NAME: &str = "alloc::boxed::Box<";

/// A type-specific formatter. Add each family as a module and register it in [`PRINTERS`].
pub trait PrettyPrinter: Sync
{
    /// Match the name or layout of a type. The first matching printer wins.
    fn matches(&self, ty: &Type, types: &TypeTable) -> bool;

    /// Render the value. Use the formatter to read memory and render children with
    /// the current limits and cycle guard.
    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String;
}

/// Built-in printers in matching order. Unmatched types use the generic formatter.
pub static PRINTERS: &[&dyn PrettyPrinter] = &[
    &enums::EnumPrinter,
    &strings::StringPrinter,
    &vec::SlicePrinter,
    &vec::RawSlicePrinter,
    &vec::VecPrinter,
    &vec::IntoIterPrinter,
    &dyn_trait::DynTraitPrinter,
    &vec::BoxPrinter,
    &shared::SharedPrinter,
    &shared::CellPrinter,
    &maps::MapPrinter,
];

fn field<'a>(value: ValueView<'a>, name: &str, types: &TypeTable) -> Option<ValueView<'a>>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    value.member(layout.member(name)?, types)
}

fn word(bytes: &[u8], endian: RunTimeEndian) -> Option<u64>
{
    if bytes.len() > 8 {
        return None;
    }
    u64::try_from(unsigned(bytes, endian)?).ok()
}

fn usize_value(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<u64>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    if ty.name.as_deref() != Some("usize") || !matches!(ty.kind, TypeKind::Base(BaseEncoding::Unsigned)) {
        return None;
    }
    word(value.bytes, endian)
}

fn thin_pointer(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<(Address, TypeId)>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Pointer(pointer) = &ty.kind else {
        return None;
    };
    Some((Address::new(word(value.bytes, endian)?), pointer.pointee?))
}

// rustc describes strings and slices with the same named data and length fields.
pub(in crate::values) fn fat_pointer(
    value: ValueView<'_>,
    types: &TypeTable,
    endian: RunTimeEndian,
) -> Option<(Address, u64, TypeId)>
{
    let (address, element) = thin_pointer(field(value, "data_ptr", types)?, types, endian)?;
    let len = usize_value(field(value, "length", types)?, types, endian)?;
    Some((address, len, element))
}

fn raw_layout(value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
{
    let types = formatter.types();
    let fields = types.get(types.strip_aliases(value.ty)).and_then(|ty| {
        let TypeKind::Struct(layout) = &ty.kind else {
            return None;
        };
        Some(formatter.format_members(value, &layout.members, ty.name.as_deref()))
    });
    let raw = fields.unwrap_or_else(|| format_raw(Ok(value.bytes)));
    format!("{raw} <unrecognized layout>")
}
