//! Render slices, raw slice and `str` pointers, vectors, vector and array `IntoIter`s, and
//! boxes using their debug info.

use gimli::RunTimeEndian;

use super::{BOX_NAME, PrettyPrinter, fat_pointer, field, raw_layout, thin_pointer, usize_value};
use crate::types::Address;
use crate::values::format::{ValueFormatter, ValueView, unavailable};
use crate::values::types::{PointerKind, Type, TypeId, TypeKind, TypeTable};

const VEC_NAME: &str = "alloc::vec::Vec<";
const INTO_ITER_NAME: &str = "alloc::vec::into_iter::IntoIter<";
const ARRAY_INTO_ITER_NAME: &str = "core::array::iter::IntoIter<";
const RAW_FAT_POINTERS: &[&str] = &["*const [", "*mut [", "*const str", "*mut str"];

pub(super) struct VecPrinter;

impl PrettyPrinter for VecPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().is_some_and(|name| name.starts_with(VEC_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some(layout) = layout(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        format_contents(&layout, "vector", formatter)
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
    let (address, capacity) = raw_buffer(value.member(layout.member("buf")?, types)?, element_size, types, endian)?;
    Some(VecLayout {
        address,
        len,
        capacity,
        element,
    })
}

/// The allocation pointer and capacity of a `RawVec` whose elements are `element_size` bytes.
pub(super) fn raw_buffer(
    buf: ValueView<'_>,
    element_size: u64,
    types: &TypeTable,
    endian: RunTimeEndian,
) -> Option<(Address, u64)>
{
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
    Some((address, capacity))
}

/// Render a `Vec`'s elements and its length and capacity. `noun` names the container in
/// the error when the length exceeds the capacity.
pub(super) fn format_contents(layout: &VecLayout, noun: &str, formatter: &mut ValueFormatter<'_>) -> String
{
    let VecLayout {
        address,
        len,
        capacity,
        element,
    } = *layout;
    if len > capacity {
        return unavailable(format!("{noun} length {len} exceeds capacity {capacity}"));
    }
    let elements = format_elements(address, len, element, formatter);
    format!("{elements} (len: {len}, capacity: {capacity})")
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

pub(super) struct RawSlicePrinter;

impl PrettyPrinter for RawSlicePrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        !matches!(ty.kind, TypeKind::Pointer(_))
            && ty
                .name
                .as_deref()
                .is_some_and(|name| RAW_FAT_POINTERS.iter().any(|prefix| name.starts_with(prefix)))
    }

    // A raw pointer may dangle, so its elements are not read.
    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some((address, len, _)) = fat_pointer(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        format!("{:#x} (len: {len})", address.value())
    }
}

pub(super) struct IntoIterPrinter;

impl PrettyPrinter for IntoIterPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(ty.kind, TypeKind::Struct(_)) && ty.name.as_deref().is_some_and(|name| name.starts_with(INTO_ITER_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some((address, len, element)) = into_iter_layout(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        format_elements(address, len, element, formatter)
    }
}

// The elements not yet yielded are `ptr..end`. For zero-sized elements `end` is `ptr` plus
// the remaining count.
fn into_iter_layout(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<(Address, u64, TypeId)>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    let element = layout.template_param("T")?.ty?;
    let size = types.get(element)?.size?;
    let pointer = |name| {
        let member = value.member(layout.member(name)?, types)?;
        let member = field(member, "pointer", types).unwrap_or(member);
        thin_pointer(member, types, endian).map(|(address, _)| address)
    };
    let (ptr, end) = (pointer("ptr")?, pointer("end")?);
    let distance = end.value().checked_sub(ptr.value())?;
    let len = if size == 0 {
        distance
    } else {
        (distance % size == 0).then_some(distance / size)?
    };
    Some((ptr, len, element))
}

pub(super) struct ArrayIntoIterPrinter;

impl PrettyPrinter for ArrayIntoIterPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(ty.kind, TypeKind::Struct(_))
            && ty.name.as_deref().is_some_and(|name| name.starts_with(ARRAY_INTO_ITER_NAME))
    }

    // The slots outside `alive` are uninitialized, so only `alive` is shown, as `T`s.
    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some((alive, len, element)) = array_into_iter_layout(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        formatter.format_sequence(Some(element), &[Some(len)], alive.address, Some(alive.bytes))
    }
}

// Recent versions hold `alive` and `data: [MaybeUninit<T>; N]` in an `inner:
// ManuallyDrop<PolymorphicIter<..>>` field.
fn array_into_iter_layout<'a>(
    value: ValueView<'a>,
    types: &TypeTable,
    endian: RunTimeEndian,
) -> Option<(ValueView<'a>, u64, TypeId)>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    let element = layout.template_param("T")?.ty?;
    let size = types.get(element)?.size?;
    let state = field(value, "inner", types).map_or(value, |inner| field(inner, "value", types).unwrap_or(inner));
    let alive = field(state, "alive", types)?;
    let start = usize_value(field(alive, "start", types)?, types, endian)?;
    let end = usize_value(field(alive, "end", types)?, types, endian)?;
    let data = field(state, "data", types)?;
    let len = end.checked_sub(start)?;
    let offset = start.checked_mul(size)?;
    let live = data.slice(element, offset, len.checked_mul(size)?)?;
    Some((live, len, element))
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

pub(super) fn format_elements(address: Address, len: u64, element: TypeId, formatter: &mut ValueFormatter<'_>) -> String
{
    if address == Address::ZERO {
        return unavailable("null element data pointer");
    }
    formatter.format_sequence(Some(element), &[Some(len)], Some(address), None)
}
