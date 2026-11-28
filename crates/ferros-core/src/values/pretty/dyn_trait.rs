//! Render Rust trait objects using their vtable's concrete type.

use std::sync::Arc;

use gimli::RunTimeEndian;

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
        let Some((data, erased, metadata)) = DynMetadata::from_pointer(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        metadata.format_at(data, erased, formatter)
    }
}

/// Metadata shared by a trait object pointer and pointers to structs with a trait object tail.
pub(super) struct DynMetadata
{
    address: Address,
    ty: TypeId,
    width: usize,
}

impl DynMetadata
{
    pub(super) fn from_pointer(
        value: ValueView<'_>,
        types: &TypeTable,
        endian: RunTimeEndian,
    ) -> Option<(Address, TypeId, Self)>
    {
        let (data, pointee) = thin_pointer(field(value, "pointer", types)?, types, endian)?;
        let field = field(value, "vtable", types)?;
        let (address, ty) = thin_pointer(field, types, endian)?;
        Some((
            data,
            pointee,
            Self {
                address,
                ty,
                width: field.bytes.len(),
            },
        ))
    }

    pub(super) fn alignment(&self, formatter: &mut ValueFormatter<'_>) -> Option<u64>
    {
        let Self { address, width, .. } = *self;
        if address == Address::ZERO {
            return None;
        }
        // The header stores drop glue, size, then alignment.
        let entry = address.checked_add(2 * width as u64)?;
        let align = word(&formatter.read_memory(entry, width).ok()?, formatter.endian())?;
        align.is_power_of_two().then_some(align)
    }

    pub(super) fn format_at(self, data: Address, erased: TypeId, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Self {
            address: vtable,
            ty: table_type,
            width,
        } = self;
        let fallback = || format!("dyn {{ data: {data}, vtable: {vtable} }}");
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
                (concrete.size.is_some() && !concrete.declaration).then(|| (types.clone(), ty, false))
            })
            .or_else(|| {
                let types = formatter.types();
                let erased = types.get(types.strip_aliases(erased))?;
                let table_type = types.get(types.strip_aliases(table_type))?;
                let (principal, entries) = method_layout(&erased, &table_type)?;
                from_methods(formatter, vtable, width, entries, principal).map(|(types, ty)| (types, ty, true))
            });
        let Some((types, ty, inferred)) = concrete else {
            return fallback();
        };
        let concrete = types.get(ty);
        let rendered = formatter.format_at_in(types, ty, data);
        let rendered = match concrete.as_ref().and_then(|ty| ty.name.as_deref()) {
            Some(name)
                if rendered != ELLIPSIS && !rendered.strip_prefix(name).is_some_and(|rest| rest.starts_with([' ', '('])) =>
            {
                format!("{name} = {rendered}")
            }
            _ => rendered,
        };
        // Optimizers can share a method body between distinct impls and retain only
        // one impl's debug info at the entry, even when the vtables belong to different types.
        if inferred && rendered != ELLIPSIS {
            format!("{rendered} (type inferred from vtable methods)")
        } else {
            rendered
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
