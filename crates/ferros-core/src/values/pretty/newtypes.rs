//! Render standard library newtypes and niche wrappers as the value they wrap.

use std::fmt::Write as _;

use gimli::RunTimeEndian;

use super::{PrettyPrinter, field, raw_layout, word};
use crate::values::format::{ValueFormatter, ValueView};
use crate::values::types::{BaseEncoding, Type, TypeKind, TypeTable};

const DURATION_NAME: &str = "core::time::Duration";
const MAYBE_UNINIT_NAME: &str = "core::mem::maybe_uninit::MaybeUninit<";
const PHANTOM_NAME: &str = "core::marker::PhantomData<";
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// How a wrapper shows the field it wraps.
#[derive(Clone, Copy)]
enum Style
{
    /// Only the wrapped value.
    Transparent,
    /// The wrapped value inside a short constructor name.
    Named(&'static str),
}

/// Wrappers that hold one field: type path (generic arguments excluded), field, style.
const WRAPPERS: &[(&str, &str, Style)] = &[
    ("core::num::nonzero::NonZero", "__0", Style::Transparent),
    ("core::mem::manually_drop::ManuallyDrop", "value", Style::Transparent),
    ("core::num::wrapping::Wrapping", "__0", Style::Named("Wrapping")),
    ("core::num::saturating::Saturating", "__0", Style::Named("Saturating")),
    ("std::thread::id::ThreadId", "__0", Style::Named("ThreadId")),
];

// rustc's scalar-valid-range types behind `NonZero`, `Duration` and `RawVec` capacities,
// such as `NonZeroU32Inner`, `Nanoseconds` and `UsizeNoHighBit`.
const NICHE_TYPES: &str = "core::num::niche_types::";

fn wrapper(name: &str) -> Option<(&'static str, Style)>
{
    if name
        .strip_prefix(NICHE_TYPES)
        .is_some_and(|rest| !rest.is_empty() && !rest.contains("::"))
    {
        return Some(("__0", Style::Transparent));
    }
    WRAPPERS
        .iter()
        .find(|(path, ..)| {
            name.strip_prefix(path)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('<'))
        })
        .map(|&(_, field, style)| (field, style))
}

pub(super) struct NewtypePrinter;

impl PrettyPrinter for NewtypePrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(ty.kind, TypeKind::Struct(_)) && ty.name.as_deref().and_then(wrapper).is_some()
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let Some((name, style)) = types.get(value.ty).and_then(|ty| ty.name.as_deref().and_then(wrapper)) else {
            return raw_layout(value, formatter);
        };
        let Some(inner) = field(value, name, types) else {
            return raw_layout(value, formatter);
        };
        let text = formatter.format(inner);
        match style {
            Style::Transparent => text,
            Style::Named(name) => format!("{name}({text})"),
        }
    }
}

/// Renders `Duration` the way its `Debug` implementation does, such as `1.5s` or `20ms`.
pub(super) struct DurationPrinter;

impl PrettyPrinter for DurationPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        matches!(ty.kind, TypeKind::Struct(_)) && ty.name.as_deref() == Some(DURATION_NAME)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let endian = formatter.endian();
        let secs = field(value, "secs", types).and_then(|secs| unsigned_value(secs, types, endian));
        let nanos = field(value, "nanos", types)
            // Older releases stored the nanoseconds as a plain `u32`.
            .map(|nanos| field(nanos, "__0", types).unwrap_or(nanos))
            .and_then(|nanos| unsigned_value(nanos, types, endian));
        match (secs, nanos) {
            (Some(secs), Some(nanos)) if nanos < NANOS_PER_SEC => format_duration(secs, nanos),
            _ => raw_layout(value, formatter),
        }
    }
}

fn unsigned_value(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<u64>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    if !matches!(ty.kind, TypeKind::Base(BaseEncoding::Unsigned)) {
        return None;
    }
    word(value.bytes, endian)
}

fn format_duration(secs: u64, nanos: u64) -> String
{
    if secs > 0 {
        decimal(u128::from(secs), nanos, NANOS_PER_SEC, "s")
    } else if nanos >= 1_000_000 {
        decimal(u128::from(nanos / 1_000_000), nanos % 1_000_000, 1_000_000, "ms")
    } else if nanos >= 1_000 {
        decimal(u128::from(nanos / 1_000), nanos % 1_000, 1_000, "µs")
    } else {
        format!("{nanos}ns")
    }
}

/// `whole.fraction` without trailing fractional zeros; `scale` is one whole unit.
fn decimal(whole: u128, fraction: u64, scale: u64, unit: &str) -> String
{
    if fraction == 0 {
        return format!("{whole}{unit}");
    }
    let width = scale.ilog10() as usize;
    let digits = format!("{fraction:0width$}");
    format!("{whole}.{}{unit}", digits.trim_end_matches('0'))
}

/// Renders `MaybeUninit` as its raw bytes, read as one hex integer in the target's byte
/// order, because the bytes need not be a valid `T`.
pub(super) struct MaybeUninitPrinter;

impl PrettyPrinter for MaybeUninitPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().is_some_and(|name| name.starts_with(MAYBE_UNINIT_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let mut text = String::from("MaybeUninit(<maybe uninit> 0x");
        let mut push = |byte: &u8| {
            let _ = write!(text, "{byte:02x}");
        };
        match formatter.endian() {
            RunTimeEndian::Little => value.bytes.iter().rev().for_each(&mut push),
            RunTimeEndian::Big => value.bytes.iter().for_each(&mut push),
        }
        text.push(')');
        text
    }
}

/// Renders `PhantomData<T>` as its type, since it has no fields.
pub(super) struct PhantomPrinter;

impl PrettyPrinter for PhantomPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().is_some_and(|name| name.starts_with(PHANTOM_NAME))
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        formatter
            .types()
            .get(value.ty)
            .and_then(|ty| Some(ty.name.as_deref()?.strip_prefix("core::marker::")?.to_owned()))
            .unwrap_or_else(|| raw_layout(value, formatter))
    }
}

#[cfg(test)]
mod tests
{
    use super::{format_duration, wrapper};

    #[test]
    fn duration_matches_debug()
    {
        use std::time::Duration;
        for (secs, nanos) in [
            (1, 500_000_000),
            (0, 0),
            (0, 20_000_000),
            (0, 1_500),
            (0, 7),
            (3, 1),
            (0, 1_000_001),
        ] {
            let expected = format!("{:?}", Duration::new(secs, nanos));
            assert_eq!(format_duration(secs, u64::from(nanos)), expected);
        }
    }

    #[test]
    fn wrapper_names_match_whole_path_segments()
    {
        assert!(wrapper("std::thread::id::ThreadId").is_some());
        assert!(wrapper("std::thread::id::ThreadIdx").is_none());
        assert!(wrapper("core::num::nonzero::NonZero<u32>").is_some());
        assert!(wrapper("core::num::nonzero::NonZeroU32").is_none());
        assert!(wrapper("core::num::niche_types::Nanoseconds").is_some());
        assert!(wrapper("core::num::wrapping::Wrapping<u8>").is_some());
    }
}
