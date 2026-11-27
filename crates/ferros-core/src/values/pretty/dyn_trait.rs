//! Render Rust trait objects using their vtable's concrete type.

use std::sync::Arc;

use super::{BOX_NAME, PrettyPrinter, field, raw_layout, thin_pointer, word};
use crate::symbols::functions::{find_top, group_end};
use crate::types::Address;
use crate::values::format::{ELLIPSIS, ValueFormatter, ValueView};
use crate::values::types::{Type, TypeId, TypeKind, TypeTable};

pub(super) struct DynTraitPrinter;

impl PrettyPrinter for DynTraitPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(ty.kind, TypeKind::Struct(_))
            && ty.name.as_deref().is_some_and(|name| {
                name.starts_with("&dyn ")
                    || name.starts_with("&mut dyn ")
                    || name.strip_prefix(BOX_NAME).is_some_and(|boxed| boxed.starts_with("dyn "))
            })
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let endian = formatter.endian();
        let Some((data, erased)) = field(value, "pointer", types).and_then(|field| thin_pointer(field, types, endian))
        else {
            return raw_layout(value, formatter);
        };
        let Some(vtable_field) = field(value, "vtable", types) else {
            return raw_layout(value, formatter);
        };
        let Some((vtable, table_type)) = thin_pointer(vtable_field, types, endian) else {
            return raw_layout(value, formatter);
        };
        let fallback = || format!("dyn {{ data: {data}, vtable: {vtable} }}");
        let erased = types.get(types.strip_aliases(erased));
        let table_type = types.get(types.strip_aliases(table_type));
        if data == Address::ZERO || vtable == Address::ZERO {
            return fallback();
        }
        let Some(image) = formatter.image_at(vtable) else {
            return fallback();
        };
        let concrete = image
            .type_table()
            .ok()
            .and_then(|types| {
                let ty = types.concrete_type_for_vtable(image.file_address(vtable)?)?;
                let concrete = types.get(ty)?;
                (concrete.size.is_some() && !concrete.declaration).then(|| (types.clone(), ty))
            })
            .or_else(|| {
                let (principal, entries) = method_layout(erased.as_deref()?, table_type.as_deref()?)?;
                from_methods(formatter, vtable, vtable_field.bytes.len(), entries, principal)
            });
        let Some((types, ty)) = concrete else {
            return fallback();
        };
        let concrete = types.get(ty);
        let rendered = formatter.format_at_in(types, ty, data);
        match concrete.as_ref().and_then(|ty| ty.name.as_deref()) {
            Some(name)
                if rendered != ELLIPSIS && !rendered.strip_prefix(name).is_some_and(|rest| rest.starts_with([' ', '('])) =>
            {
                format!("{name} = {rendered}")
            }
            _ => rendered,
        }
    }
}

fn method_layout<'a>(erased: &'a Type, table: &Type) -> Option<(&'a str, usize)>
{
    let principal = erased.name.as_deref()?.strip_prefix("dyn ")?;
    let principal = find_top(principal, " + ").map_or(principal, |end| &principal[..end]);
    let TypeKind::Array(layout) = &table.kind else {
        return None;
    };
    let [Some(entries)] = layout.lengths.as_slice() else {
        return None;
    };
    Some((principal, usize::try_from(*entries).ok()?))
}

// On builds without relocated vtable DIEs, look for a method of the principal trait.
// The rustc vtable begins with drop glue, size and alignment. Remaining words may also
// contain supertrait vtables, so require an exact function entry and a matching impl.
fn from_methods(
    formatter: &mut ValueFormatter<'_>,
    vtable: Address,
    width: usize,
    entries: usize,
    principal: &str,
) -> Option<(Arc<TypeTable>, TypeId)>
{
    if entries <= 3 {
        return None;
    }
    let bytes = formatter.read_memory(vtable, width.checked_mul(entries)?).ok()?;
    let mut entries = bytes.chunks_exact(width);
    let endian = formatter.endian();
    let size = word(entries.nth(1)?, endian)?;
    let align = word(entries.next()?, endian)?;
    let mut concrete: Option<(Arc<TypeTable>, TypeId)> = None;
    for entry in entries {
        let address = Address::new(word(entry, endian)?);
        let Some((types, ty)) = method_type(formatter, address, principal, size, align) else {
            continue;
        };
        if concrete
            .as_ref()
            .is_some_and(|(previous, id)| !Arc::ptr_eq(previous, &types) || *id != ty)
        {
            return None;
        }
        concrete = Some((types, ty));
    }
    concrete
}

fn method_type(
    formatter: &mut ValueFormatter<'_>,
    address: Address,
    principal: &str,
    size: u64,
    align: u64,
) -> Option<(Arc<TypeTable>, TypeId)>
{
    let image = formatter.image_at(address)?;
    let name = image.function_name_at(address)?;
    let end = group_end(name)?;
    let qualifier = name
        .get(1..end)
        .filter(|_| name.starts_with('<') && name[end + 1..].starts_with("::"))?;
    let separator = find_top(qualifier, " as ")?;
    if &qualifier[separator + " as ".len()..] != principal {
        return None;
    }
    let types = image.type_table().ok()?;
    let ty = types.type_named(&qualifier[..separator])?;
    let concrete = types.get(ty)?;
    (!concrete.declaration && concrete.size == Some(size) && concrete.alignment.is_none_or(|alignment| alignment == align))
        .then(|| (types.clone(), ty))
}
