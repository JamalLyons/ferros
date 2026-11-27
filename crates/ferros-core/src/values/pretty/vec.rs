//! Render slices, vectors and boxes using their debug info.

use gimli::RunTimeEndian;

use super::{BOX_NAME, PrettyPrinter, fat_pointer, field, raw_layout, thin_pointer, usize_value};
use crate::types::Address;
use crate::values::format::{ValueFormatter, ValueView, unavailable};
use crate::values::types::{PointerKind, Type, TypeId, TypeKind, TypeTable};

const VEC_NAME: &str = "alloc::vec::Vec<";

pub(super) struct VecPrinter;

impl PrettyPrinter for VecPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().is_some_and(|name| name.starts_with(VEC_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some(VecLayout {
            address,
            len,
            capacity,
            element,
        }) = layout(value, formatter.types(), formatter.endian())
        else {
            return raw_layout(value, formatter);
        };
        if len > capacity {
            return unavailable(format!("vector length {len} exceeds capacity {capacity}"));
        }
        let elements = format_elements(address, len, element, formatter);
        format!("{elements} (len: {len}, capacity: {capacity})")
    }
}

pub(in crate::values) struct VecLayout
{
    pub(in crate::values) address: Address,
    pub(in crate::values) len: u64,
    pub(in crate::values) capacity: u64,
    pub(in crate::values) element: TypeId,
}

pub(in crate::values) fn layout(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<VecLayout>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    if !ty.name.as_deref()?.starts_with(VEC_NAME) {
        return None;
    }
    let element = layout.template_param("T")?.ty?;
    let element_size = types.get(element)?.size?;
    let len = usize_value(value.member(layout.member("len")?, types)?, types, endian)?;
    let buf = value.member(layout.member("buf")?, types)?;
    // Recent RawVecs place the allocation fields in RawVecInner.
    let raw = field(buf, "inner", types).unwrap_or(buf);
    let ptr = field(raw, "ptr", types)?;
    // The raw pointer may be wrapped in Unique and/or NonNull.
    let ptr = field(ptr, "pointer", types).unwrap_or(ptr);
    let ptr = field(ptr, "pointer", types).unwrap_or(ptr);
    let (address, _) = thin_pointer(ptr, types, endian)?;
    let cap = field(raw, "cap", types)?;
    let cap = field(cap, "__0", types).unwrap_or(cap);
    let capacity = usize_value(cap, types, endian)?;
    let capacity = if element_size == 0 {
        // Vec uses usize::MAX for zero-sized elements, irrespective of the stored capacity.
        u64::MAX >> (8 * (8 - cap.bytes.len()))
    } else {
        capacity
    };
    Some(VecLayout {
        address,
        len,
        capacity,
        element,
    })
}

pub(super) struct SlicePrinter;

impl PrettyPrinter for SlicePrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        // References and boxes of sized arrays are thin pointers.
        !matches!(ty.kind, TypeKind::Pointer(_))
            && ty.name.as_deref().is_some_and(|name| {
                name.starts_with("&[")
                    || name.starts_with("&mut [")
                    || name.strip_prefix(BOX_NAME).is_some_and(|boxed| boxed.starts_with('['))
            })
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some((address, len, element)) = fat_pointer(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        format_elements(address, len, element, formatter)
    }
}

pub(super) struct BoxPrinter;

impl PrettyPrinter for BoxPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(&ty.kind, TypeKind::Pointer(pointer) if pointer.kind == PointerKind::Box)
            || ty.name.as_deref().is_some_and(|name| name.starts_with(BOX_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some((address, pointee)) = thin_pointer(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        if address == Address::ZERO {
            return unavailable("null Box pointer");
        }
        formatter.format_at(pointee, address)
    }
}

fn format_elements(address: Address, len: u64, element: TypeId, formatter: &mut ValueFormatter<'_>) -> String
{
    if address == Address::ZERO {
        return unavailable("null element data pointer");
    }
    formatter.format_sequence(Some(element), &[Some(len)], Some(address), None)
}
