//! Render the active variant described by DWARF, and find it for `print <path>`.

use std::fmt;
use std::sync::Arc;

use gimli::RunTimeEndian;

use super::PrettyPrinter;
use crate::values::format::{ValueFormatter, ValueView, unavailable, unsigned};
use crate::values::types::{Discriminant, Member, RustEnum, Type, TypeKind, TypeTable, Variant};

pub(super) struct EnumPrinter;

impl PrettyPrinter for EnumPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(ty.kind, TypeKind::Enum(_) | TypeKind::Enumeration(_))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some(ty) = formatter.types().get(value.ty) else {
            return unavailable("enum type unknown");
        };
        match &ty.kind {
            TypeKind::Enum(layout) => format_rust_enum(value, ty.name.as_deref(), layout, formatter),
            TypeKind::Enumeration(layout) => {
                let Some(raw) = unsigned(value.bytes, formatter.endian()) else {
                    return unavailable("enum discriminant bytes invalid");
                };
                layout
                    .enumerators
                    .iter()
                    .filter(|enumerator| enumerator.value == Some(raw))
                    .find_map(|enumerator| enumerator.name.clone())
                    .unwrap_or_else(|| unknown_variant(raw))
            }
            _ => unavailable("enum layout unknown"),
        }
    }
}

fn format_rust_enum(
    value: ValueView<'_>,
    enum_name: Option<&str>,
    layout: &RustEnum,
    formatter: &mut ValueFormatter<'_>,
) -> String
{
    match active_variant(value, layout, formatter.types(), formatter.endian()) {
        Ok(variant) => format_variant(value, enum_name, variant, formatter),
        Err(VariantError::Unknown(raw)) => unknown_variant(raw),
        Err(VariantError::Unreadable(reason)) => unavailable(reason),
    }
}

/// Why [`active_variant`] found no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::values) enum VariantError
{
    /// The discriminant's layout or bytes cannot be read.
    Unreadable(&'static str),
    /// No variant claims the discriminant's raw bits.
    Unknown(u128),
}

impl fmt::Display for VariantError
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        match self {
            Self::Unreadable(reason) => f.write_str(reason),
            Self::Unknown(raw) => write!(f, "unknown variant: {raw:#x}"),
        }
    }
}

/// The variant `value`, an enum with `layout`, holds: the one its discriminant (an explicit
/// tag or a niche) selects, or the only variant of an enum without a discriminant.
fn active_variant<'l>(
    value: ValueView<'_>,
    layout: &'l RustEnum,
    types: &TypeTable,
    endian: RunTimeEndian,
) -> Result<&'l Variant, VariantError>
{
    let tag = layout
        .discriminant
        .as_ref()
        .and_then(|discriminant| value.member(discriminant, types))
        .map(|tag| tag.bytes);
    variant_for_tag(layout, tag, endian)
}

/// The variant `tag`, the bytes of `layout.discriminant`, selects, for a caller that read
/// only those bytes. `tag` is ignored, and may be `None`, when the enum has no
/// discriminant; `None` for an enum with one is unreadable.
pub(in crate::values) fn variant_for_tag<'l>(
    layout: &'l RustEnum,
    tag: Option<&[u8]>,
    endian: RunTimeEndian,
) -> Result<&'l Variant, VariantError>
{
    if layout.discriminant.is_some() {
        let raw = tag
            .and_then(|tag| unsigned(tag, endian))
            .ok_or(VariantError::Unreadable("enum discriminant layout or bytes unknown"))?;
        return layout.variant_for(raw).ok_or(VariantError::Unknown(raw));
    }
    match layout.variants.as_slice() {
        [variant] if variant.discriminant == Discriminant::Default => Ok(variant),
        _ => Err(VariantError::Unreadable("enum has no readable discriminant")),
    }
}

/// The struct rustc wraps a variant's fields in, named after the variant.
pub(in crate::values) struct VariantPayload<'v>
{
    /// The variant's one member, whose type the payload struct is.
    pub(in crate::values) member: &'v Member,
    ty: Arc<Type>,
}

impl VariantPayload<'_>
{
    /// The payload struct's fields, at offsets from [`member`](Self::member).
    pub(in crate::values) fn fields(&self) -> &[Member]
    {
        match &self.ty.kind {
            TypeKind::Struct(payload) => &payload.members,
            _ => &[],
        }
    }
}

/// The payload struct of `variant`, or `None` when the variant is not described that way.
pub(in crate::values) fn variant_payload<'v>(
    variant: &'v Variant,
    enum_name: Option<&str>,
    types: &TypeTable,
) -> Option<VariantPayload<'v>>
{
    let ([member], Some(enum_name)) = (variant.members.as_slice(), enum_name) else {
        return None;
    };
    let ty = types.get(member.ty?)?;
    let qualified = ty.name.as_deref()?;
    if qualified.strip_prefix(enum_name)?.strip_prefix("::") != Some(variant.name.as_deref()?) || ty.declaration {
        return None;
    }
    if !matches!(ty.kind, TypeKind::Struct(_)) {
        return None;
    }
    Some(VariantPayload { member, ty })
}

fn format_variant(
    value: ValueView<'_>,
    enum_name: Option<&str>,
    variant: &Variant,
    formatter: &mut ValueFormatter<'_>,
) -> String
{
    let name = variant.name.as_deref().unwrap_or("?");
    // Flatten the payload wrapper so the field formatter applies the enum's depth and
    // element limits.
    if let Some(payload) = variant_payload(variant, enum_name, formatter.types()) {
        if payload.fields().is_empty() {
            return name.to_owned();
        }
        let Some(payload_view) = value.member(payload.member, formatter.types()) else {
            return unavailable("variant payload layout or bytes unknown");
        };
        return formatter.format_members(payload_view, payload.fields(), Some(name));
    }
    if variant.members.is_empty() {
        return name.to_owned();
    }
    formatter.format_members(value, &variant.members, Some(name))
}

fn unknown_variant(raw: u128) -> String
{
    format!("<unknown variant: {raw:#x}>")
}
