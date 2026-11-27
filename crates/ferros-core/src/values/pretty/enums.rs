//! Render the active variant described by DWARF.

use super::PrettyPrinter;
use crate::values::format::{ValueFormatter, ValueView, unavailable, unsigned};
use crate::values::types::{Discriminant, RustEnum, Type, TypeKind, TypeTable, Variant};

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
    let variant = if let Some(discriminant) = &layout.discriminant {
        let Some(raw) = value
            .member(discriminant, formatter.types())
            .and_then(|tag| unsigned(tag.bytes, formatter.endian()))
        else {
            return unavailable("enum discriminant layout or bytes unknown");
        };
        let Some(variant) = layout.variant_for(raw) else {
            return unknown_variant(raw);
        };
        variant
    } else {
        let [variant] = layout.variants.as_slice() else {
            return unavailable("enum has no readable discriminant");
        };
        if variant.discriminant != Discriminant::Default {
            return unavailable("enum has no readable discriminant");
        }
        variant
    };
    format_variant(value, enum_name, variant, formatter)
}

fn format_variant(
    value: ValueView<'_>,
    enum_name: Option<&str>,
    variant: &Variant,
    formatter: &mut ValueFormatter<'_>,
) -> String
{
    let name = variant.name.as_deref().unwrap_or("?");
    // rustc wraps each variant's fields in a nested struct named after the variant.
    // Flatten that wrapper so the field formatter applies the enum's depth and element limits.
    if let ([member], Some(enum_name)) = (variant.members.as_slice(), enum_name)
        && let Some(ty) = member.ty.and_then(|id| formatter.types().get(id))
        && ty
            .name
            .as_deref()
            .and_then(|qualified| qualified.strip_prefix(enum_name)?.strip_prefix("::"))
            == Some(name)
        && !ty.declaration
        && let TypeKind::Struct(payload) = &ty.kind
    {
        if payload.members.is_empty() {
            return name.to_owned();
        }
        let Some(payload_view) = value.member(member, formatter.types()) else {
            return unavailable("variant payload layout or bytes unknown");
        };
        return formatter.format_members(payload_view, &payload.members, Some(name));
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
