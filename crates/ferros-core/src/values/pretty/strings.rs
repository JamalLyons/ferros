//! Render Rust strings from a bounded prefix of their UTF-8 bytes.

use super::{BOX_NAME, PrettyPrinter, fat_pointer, field, raw_layout, vec};
use crate::types::Address;
use crate::values::format::{ELLIPSIS, ValueFormatter, ValueView, unavailable};
use crate::values::types::{BaseEncoding, Type, TypeKind, TypeTable};

const MAX_STRING_BYTES: u64 = 256;
const STRING_NAME: &str = "alloc::string::String";

pub(super) struct StringPrinter;

impl PrettyPrinter for StringPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().is_some_and(|name| {
            matches!(name, STRING_NAME | "&str" | "&mut str")
                || name
                    .strip_prefix(BOX_NAME)
                    .is_some_and(|boxed| boxed == "str>" || boxed.starts_with("str,"))
        })
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let endian = formatter.endian();
        let owned = types.get(value.ty).is_some_and(|ty| ty.name.as_deref() == Some(STRING_NAME));
        let layout = if owned {
            field(value, "vec", types)
                .and_then(|field| vec::layout(field, types, endian))
                .filter(|vec| vec.len <= vec.capacity)
                .map(|vec| (vec.address, vec.len, vec.element))
        } else {
            fat_pointer(value, types, endian)
        };
        let Some((address, len, element)) = layout else {
            return raw_layout(value, formatter);
        };
        let is_byte = types.get(types.strip_aliases(element)).is_some_and(|ty| {
            ty.name.as_deref() == Some("u8")
                && ty.size == Some(1)
                && matches!(ty.kind, TypeKind::Base(BaseEncoding::Unsigned | BaseEncoding::UnsignedChar))
        });
        if !is_byte {
            return raw_layout(value, formatter);
        }
        format_string(address, len, formatter)
    }
}

fn format_string(address: Address, len: u64, formatter: &mut ValueFormatter<'_>) -> String
{
    if address == Address::ZERO {
        return unavailable("null string data pointer");
    }
    let truncated = len > MAX_STRING_BYTES;
    let bytes = match formatter.read_memory(address, len.min(MAX_STRING_BYTES) as usize) {
        Ok(bytes) => bytes,
        Err(reason) => return unavailable(reason),
    };
    let text = match std::str::from_utf8(&bytes) {
        Err(error) if truncated && error.error_len().is_none() => std::str::from_utf8(&bytes[..error.valid_up_to()]),
        result => result,
    };
    match text {
        Ok(text) if truncated => format!("{text:?}{ELLIPSIS}"),
        Ok(text) => format!("{text:?}"),
        Err(error) => unavailable(error),
    }
}
