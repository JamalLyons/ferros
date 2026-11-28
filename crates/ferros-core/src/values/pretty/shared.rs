//! Render Rust shared pointers and interior-mutable cells from named DWARF fields.

use gimli::RunTimeEndian;

use super::{PrettyPrinter, fat_pointer, field, raw_layout, strings, thin_pointer, usize_value, word};
use crate::types::Address;
use crate::values::format::{ValueFormatter, ValueView, unavailable};
use crate::values::location::Unavailable;
use crate::values::types::{BaseEncoding, Member, Type, TypeId, TypeKind, TypeTable};

const RC_NAME: &str = "alloc::rc::Rc<";
const ARC_NAME: &str = "alloc::sync::Arc<";
const CELL_NAME: &str = "core::cell::Cell<";
const REF_CELL_NAME: &str = "core::cell::RefCell<";

#[derive(Clone, Copy)]
enum SharedKind
{
    Rc,
    Arc,
}

impl SharedKind
{
    fn from_name(name: &str) -> Option<(Self, &str)>
    {
        if let Some(args) = name.strip_prefix(RC_NAME) {
            Some((Self::Rc, args))
        } else {
            name.strip_prefix(ARC_NAME).map(|args| (Self::Arc, args))
        }
    }

    fn read_counter(self, value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<Count>
    {
        let word = match self {
            Self::Rc => cell_value(value, types)?,
            Self::Arc => {
                let inner = field(field(value, "v", types)?, "value", types)?;
                // Atomic<usize> wraps the word in an alignment newtype. AtomicUsize used
                // the word directly.
                field(inner, "__0", types).unwrap_or(inner)
            }
        };
        Some(Count {
            raw: usize_value(word, types, endian)?,
            is_max: word.bytes.iter().all(|&byte| byte == u8::MAX),
        })
    }
}

pub(super) struct SharedPrinter;

impl PrettyPrinter for SharedPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().and_then(SharedKind::from_name).is_some()
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some(layout) = shared_layout(value, formatter.types(), formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        if layout.base == Address::ZERO {
            return unavailable("null shared allocation pointer");
        }
        let counts = layout.strong.read(layout.base, layout.kind, formatter).and_then(|strong| {
            let weak = layout.weak.read(layout.base, layout.kind, formatter)?;
            Ok(strong.zip(weak))
        });
        let (strong, weak) = match counts {
            Ok(Some((strong, weak))) => (strong.raw, weak),
            Ok(None) => return raw_layout(value, formatter),
            Err(reason) => return unavailable(reason),
        };
        if strong == 0 {
            return unavailable("shared allocation has no strong owners");
        }
        // The strong owners collectively hold one implicit weak reference. An Arc may
        // temporarily lock that sole weak reference with the target's usize::MAX.
        let weak = if matches!(layout.kind, SharedKind::Arc) && weak.is_max {
            0
        } else if let Some(weak) = weak.raw.checked_sub(1) {
            weak
        } else {
            return unavailable("shared allocation is missing its implicit weak reference");
        };
        let Some(address) = layout.base.checked_add(layout.data_offset) else {
            return unavailable("shared pointee address overflows");
        };
        let pointee = match layout.pointee {
            Pointee::Sized(ty) => formatter.format_at(ty, address),
            Pointee::Str(len) => strings::format_string(address, len, formatter),
            Pointee::Slice(element, len) => formatter.format_sequence(Some(element), &[Some(len)], Some(address), None),
        };
        let name = match layout.kind {
            SharedKind::Rc => "Rc",
            SharedKind::Arc => "Arc",
        };
        format!("{name}(strong: {strong}, weak: {weak}) {pointee}")
    }
}

struct SharedLayout
{
    kind: SharedKind,
    base: Address,
    strong: Counter,
    weak: Counter,
    data_offset: u64,
    pointee: Pointee,
}

pub(in crate::values) enum Pointee
{
    Sized(TypeId),
    Str(u64),
    Slice(TypeId, u64),
}

fn shared_layout(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<SharedLayout>
{
    let ty = types.get(value.ty)?;
    let name = ty.name.as_deref()?;
    let (kind, args) = SharedKind::from_name(name).or_else(|| SharedKind::from_weak_name(name))?;
    let non_null = field(value, "ptr", types)?;
    let pointer = field(non_null, "pointer", types)?;
    let (base, inner, len) = match thin_pointer(pointer, types, endian) {
        Some((base, inner)) => (base, inner, None),
        None => {
            let (base, len, inner) = fat_pointer(pointer, types, endian)?;
            (base, inner, Some(len))
        }
    };
    let inner = types.get(types.strip_aliases(inner))?;
    let TypeKind::Struct(fields) = &inner.kind else {
        return None;
    };
    let payload = fields.member(match kind {
        SharedKind::Rc => "value",
        SharedKind::Arc => "data",
    })?;
    if payload.bit_size.is_some() {
        return None;
    }
    let ty = payload.ty?;
    let pointee = match len {
        None if types.get(ty)?.size.is_some() => Pointee::Sized(ty),
        Some(len) => {
            let ty = types.get(types.strip_aliases(ty))?;
            let TypeKind::Array(array) = &ty.kind else {
                return None;
            };
            if array.lengths != [None] {
                return None;
            }
            let element = array.element?;
            if args == "str>" || args.starts_with("str,") {
                let byte = types.get(types.strip_aliases(element))?;
                if byte.name.as_deref() != Some("u8") || byte.size != Some(1) {
                    return None;
                }
                Pointee::Str(len)
            } else {
                Pointee::Slice(element, len)
            }
        }
        None => return None,
    };
    Some(SharedLayout {
        kind,
        base,
        strong: Counter::new(fields.member("strong")?, types)?,
        weak: Counter::new(fields.member("weak")?, types)?,
        data_offset: payload.offset?,
        pointee,
    })
}

impl SharedKind
{
    /// The kind of shared pointer a `Weak` downgrades, and its generic arguments.
    fn from_weak_name(name: &str) -> Option<(Self, &str)>
    {
        const RC_WEAK_NAME: &str = "alloc::rc::Weak<";
        const ARC_WEAK_NAME: &str = "alloc::sync::Weak<";
        if let Some(args) = name.strip_prefix(RC_WEAK_NAME) {
            Some((Self::Rc, args))
        } else {
            name.strip_prefix(ARC_WEAK_NAME).map(|args| (Self::Arc, args))
        }
    }
}

/// Where an `Rc`, `Arc` or `Weak` points, for a caller that follows it.
pub(in crate::values) enum SharedTarget
{
    /// The allocation pointer is null.
    Null,
    /// No strong owner keeps the payload alive: a `Weak` from `Weak::new`, or one whose
    /// last strong owner was dropped.
    Dangling,
    /// The payload at `address`.
    Live
    {
        address: Address, pointee: Pointee
    },
}

/// Follow `value`, an `Rc`, `Arc` or `Weak`, reading its strong count with `read`. `None`
/// when `value` is none of those or its layout is not recognized.
pub(in crate::values) fn follow_shared(
    value: ValueView<'_>,
    types: &TypeTable,
    endian: RunTimeEndian,
    read: &mut dyn FnMut(Address, usize) -> Result<Vec<u8>, String>,
) -> Option<Result<SharedTarget, String>>
{
    let layout = shared_layout(value, types, endian)?;
    if layout.base == Address::ZERO {
        return Some(Ok(SharedTarget::Null));
    }
    // `Weak::new` stores usize::MAX, which never addresses an allocation.
    if layout.base.value() == u64::MAX || layout.base.value() == u64::from(u32::MAX) {
        return Some(Ok(SharedTarget::Dangling));
    }
    let Some(counter) = layout.base.checked_add(layout.strong.offset) else {
        return Some(Err("shared counter address overflows".to_owned()));
    };
    let bytes = match read(counter, layout.strong.size) {
        Ok(bytes) => bytes,
        Err(reason) => return Some(Err(reason)),
    };
    let view = ValueView {
        ty: layout.strong.ty,
        bytes: &bytes,
        address: Some(counter),
    };
    let strong = layout.kind.read_counter(view, types, endian)?;
    if strong.raw == 0 {
        return Some(Ok(SharedTarget::Dangling));
    }
    let Some(address) = layout.base.checked_add(layout.data_offset) else {
        return Some(Err("shared pointee address overflows".to_owned()));
    };
    Some(Ok(SharedTarget::Live {
        address,
        pointee: layout.pointee,
    }))
}

struct Counter
{
    ty: TypeId,
    offset: u64,
    size: usize,
}

struct Count
{
    raw: u64,
    is_max: bool,
}

impl Counter
{
    fn new(member: &Member, types: &TypeTable) -> Option<Self>
    {
        let ty = member.ty?;
        let size = types.get(ty)?.size?;
        if member.bit_size.is_some() || !(1..=8).contains(&size) {
            return None;
        }
        Some(Self {
            ty,
            offset: member.offset?,
            size: size as usize,
        })
    }

    fn read(&self, base: Address, kind: SharedKind, formatter: &mut ValueFormatter<'_>)
    -> Result<Option<Count>, Unavailable>
    {
        let address = base
            .checked_add(self.offset)
            .ok_or_else(|| Unavailable::Memory("shared counter address overflows".to_owned()))?;
        let bytes = formatter.read_memory(address, self.size)?;
        let view = ValueView {
            ty: self.ty,
            bytes: &bytes,
            address: Some(address),
        };
        Ok(kind.read_counter(view, formatter.types(), formatter.endian()))
    }
}

pub(super) struct CellPrinter;

impl PrettyPrinter for CellPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name
            .as_deref()
            .is_some_and(|name| name.starts_with(CELL_NAME) || name.starts_with(REF_CELL_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let Some(child) = cell_value(value, types) else {
            return raw_layout(value, formatter);
        };
        let ref_cell = types
            .get(value.ty)
            .is_some_and(|ty| ty.name.as_deref().is_some_and(|name| name.starts_with(REF_CELL_NAME)));
        if !ref_cell {
            return formatter.format(child);
        }
        let Some(state) = borrow_state(value, types, formatter.endian()) else {
            return raw_layout(value, formatter);
        };
        format!("{} ({state})", formatter.format(child))
    }
}

fn cell_value<'a>(cell: ValueView<'a>, types: &TypeTable) -> Option<ValueView<'a>>
{
    field(field(cell, "value", types)?, "value", types)
}

fn borrow_state(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<String>
{
    let flag = cell_value(field(value, "borrow", types)?, types)?;
    let ty = types.get(types.strip_aliases(flag.ty))?;
    if ty.name.as_deref() != Some("isize") || !matches!(ty.kind, TypeKind::Base(BaseEncoding::Signed)) {
        return None;
    }
    let raw = word(flag.bytes, endian)?;
    let sign_bit = 1 << (flag.bytes.len() * 8 - 1);
    Some(match raw {
        0 => "unborrowed".to_owned(),
        raw if raw & sign_bit != 0 => "mutably borrowed".to_owned(),
        raw => format!("shared borrows: {raw}"),
    })
}
