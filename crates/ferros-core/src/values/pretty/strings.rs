//! Render Rust strings from a bounded prefix of their bytes.
//!
//! `String` and `str` must hold UTF-8. FFI and OS strings (`CString`, `CStr`, `OsString`,
//! `OsStr`, `PathBuf`, `Path`) may hold any bytes, so bytes that are not UTF-8 print as
//! `\xNN` escapes.

use std::fmt::Write as _;

use super::{BOX_NAME, PrettyPrinter, fat_pointer, field, raw_layout, vec};
use crate::types::Address;
use crate::values::format::{ELLIPSIS, ValueFormatter, ValueView, unavailable};
use crate::values::types::{BaseEncoding, Type, TypeId, TypeKind, TypeTable};

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
            let Some(vec) = field(value, "vec", types).and_then(|field| vec::layout(field, types, endian)) else {
                return raw_layout(value, formatter);
            };
            if vec.len > vec.capacity {
                return unavailable(format!("string length {} exceeds capacity {}", vec.len, vec.capacity));
            }
            Some((vec.address, vec.len, vec.element))
        } else {
            fat_pointer(value, types, endian)
        };
        let Some((address, len, element)) = layout else {
            return raw_layout(value, formatter);
        };
        let Some(element) = unsized_element(element, types) else {
            return raw_layout(value, formatter);
        };
        let is_byte = types.get(element).is_some_and(|ty| {
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

pub(super) fn format_string(address: Address, len: u64, formatter: &mut ValueFormatter<'_>) -> String
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

const C_STRING_NAME: &str = "alloc::ffi::c_str::CString";
const C_STR_NAME: &str = "core::ffi::c_str::CStr";
const OS_STRING_NAME: &str = "std::ffi::os_str::OsString";
const OS_STR_NAME: &str = "std::ffi::os_str::OsStr";
const PATH_BUF_NAME: &str = "std::path::PathBuf";
const PATH_NAME: &str = "std::path::Path";
// Wrapper fields between an owned string and its byte buffer: `PathBuf` holds an
// `OsString`, which holds a `Buf`, which holds a `Vec<u8>`.
const MAX_INNER_DEPTH: usize = 4;

/// Renders FFI and OS strings as quoted text: `c"..."` for C strings, `"..."` otherwise.
pub(super) struct ByteStringPrinter;

impl ByteStringPrinter
{
    /// Whether the named type is a C string, or `None` when it is no FFI or OS string.
    fn kind(name: &str) -> Option<bool>
    {
        let unsized_name = name.strip_prefix("&mut ").or_else(|| name.strip_prefix('&')).or_else(|| {
            name.strip_prefix(BOX_NAME)
                .map(|boxed| boxed.split([',', '>']).next().unwrap_or(boxed))
        });
        match (name, unsized_name) {
            (C_STRING_NAME, _) | (_, Some(C_STR_NAME)) => Some(true),
            (OS_STRING_NAME | PATH_BUF_NAME, _) | (_, Some(OS_STR_NAME | PATH_NAME)) => Some(false),
            _ => None,
        }
    }
}

impl PrettyPrinter for ByteStringPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        ty.name.as_deref().and_then(Self::kind).is_some()
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let endian = formatter.endian();
        let Some(is_c) = types.get(value.ty).and_then(|ty| ty.name.as_deref().and_then(Self::kind)) else {
            return raw_layout(value, formatter);
        };
        let mut current = value;
        let mut layout = None;
        for _ in 0..MAX_INNER_DEPTH {
            if let Some(vec) = vec::layout(current, types, endian) {
                if vec.len > vec.capacity {
                    return unavailable(format!("string length {} exceeds capacity {}", vec.len, vec.capacity));
                }
                layout = Some((vec.address, vec.len, vec.element));
                break;
            }
            if let Some(slice) = fat_pointer(current, types, endian) {
                layout = Some(slice);
                break;
            }
            let Some(inner) = field(current, "inner", types) else {
                break;
            };
            current = inner;
        }
        let Some((address, len, element)) = layout else {
            return raw_layout(value, formatter);
        };
        let Some(element) = unsized_element(element, types) else {
            return raw_layout(value, formatter);
        };
        let is_byte = types.get(element).is_some_and(|ty| {
            ty.size == Some(1)
                && matches!(
                    ty.kind,
                    TypeKind::Base(
                        BaseEncoding::Unsigned
                            | BaseEncoding::UnsignedChar
                            | BaseEncoding::Signed
                            | BaseEncoding::SignedChar
                    )
                )
        });
        if !is_byte {
            return raw_layout(value, formatter);
        }
        if is_c {
            // A C string's length counts its NUL terminator.
            let Some(len) = len.checked_sub(1) else {
                return unavailable("C string has no NUL terminator");
            };
            format!("c{}", format_bytes(address, len, formatter))
        } else {
            format_bytes(address, len, formatter)
        }
    }
}

/// The byte type behind a borrowed FFI or OS string's pointee. A reference to `Path`,
/// `OsStr` or `CStr` points at the unsized struct, whose `inner` fields at offset zero
/// end in a byte slice. Other element types are returned unchanged.
fn unsized_element(element: TypeId, types: &TypeTable) -> Option<TypeId>
{
    let mut current = types.strip_aliases(element);
    for _ in 0..MAX_INNER_DEPTH {
        match &types.get(current)?.kind {
            TypeKind::Struct(layout) => {
                let inner = layout.member("inner")?;
                if inner.offset != Some(0) || inner.bit_size.is_some() {
                    return None;
                }
                current = types.strip_aliases(inner.ty?);
            }
            TypeKind::Array(array) if array.lengths == [None] => return array.element.map(|ty| types.strip_aliases(ty)),
            _ => return Some(current),
        }
    }
    None
}

/// Render bytes as a quoted string, escaping bytes that are not UTF-8 as `\xNN`.
fn format_bytes(address: Address, len: u64, formatter: &mut ValueFormatter<'_>) -> String
{
    if len == 0 {
        return "\"\"".to_owned();
    }
    if address == Address::ZERO {
        return unavailable("null string data pointer");
    }
    let truncated = len > MAX_STRING_BYTES;
    let bytes = match formatter.read_memory(address, len.min(MAX_STRING_BYTES) as usize) {
        Ok(bytes) => bytes,
        Err(reason) => return unavailable(reason),
    };
    let mut text = String::from("\"");
    let mut chunks = bytes.utf8_chunks().peekable();
    while let Some(chunk) = chunks.next() {
        let valid = format!("{:?}", chunk.valid());
        text.push_str(&valid[1..valid.len() - 1]);
        let invalid = chunk.invalid();
        // A character the preview limit cuts short is not an invalid byte.
        let cut_short = truncated
            && chunks.peek().is_none()
            && std::str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none());
        if !cut_short {
            for byte in invalid {
                let _ = write!(text, "\\x{byte:02x}");
            }
        }
    }
    text.push('"');
    if truncated {
        text.push_str(ELLIPSIS);
    }
    text
}
