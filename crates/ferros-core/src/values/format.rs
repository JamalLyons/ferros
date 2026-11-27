//! Render typed target values, with bounded expansion and target byte order.

use std::collections::HashSet;
use std::fmt::{Display, Write as _};

use gimli::RunTimeEndian;

use super::location::{Unavailable, value_size};
use super::pretty::{PRINTERS, PrettyPrinter};
use super::types::{BaseEncoding, Member, PointerKind, PointerType, Type, TypeId, TypeKind, TypeTable};
use crate::error::Result as DebuggerResult;
use crate::types::Address;

pub(super) const ELLIPSIS: &str = "...";
// Depth and per-aggregate limits alone allow exponential expansion of wide, nested values.
const MAX_NODES: usize = 1024;

/// Limits on rendering one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOptions
{
    /// Maximum number of levels shown, including the root.
    pub max_depth: usize,
    /// Maximum number of children shown per aggregate.
    pub max_elements: usize,
}

impl Default for FormatOptions
{
    fn default() -> Self
    {
        Self {
            max_depth: 8,
            max_elements: 16,
        }
    }
}

/// Readable bytes of a value, in target memory order.
#[derive(Debug, Clone, Copy)]
pub struct ValueView<'a>
{
    /// The type id in the formatter's image table.
    pub ty: TypeId,
    /// The value's bytes.
    pub bytes: &'a [u8],
    /// Runtime address of the first byte, when the value is backed by memory.
    pub address: Option<Address>,
}

impl<'a> ValueView<'a>
{
    /// View an inline child at `offset`, keeping its address when known.
    #[must_use]
    pub fn slice(self, ty: TypeId, offset: u64, size: u64) -> Option<Self>
    {
        let end = usize::try_from(offset.checked_add(size)?).ok()?;
        let bytes = self.bytes.get(usize::try_from(offset).ok()?..end)?;
        let address = match self.address {
            Some(address) => Some(address.checked_add(offset)?),
            None => None,
        };
        Some(Self { ty, bytes, address })
    }

    pub(super) fn member(self, member: &Member, types: &TypeTable) -> Option<Self>
    {
        if member.bit_size.is_some() {
            return None;
        }
        let ty = member.ty?;
        self.slice(ty, member.offset?, types.get(ty)?.size?)
    }
}

/// Formats values from one image and reads additional bytes from the stopped target.
pub struct ValueFormatter<'a>
{
    types: &'a TypeTable,
    endian: RunTimeEndian,
    options: FormatOptions,
    read: &'a mut dyn FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
    printers: &'a [&'a dyn PrettyPrinter],
    // Printers can follow links and render a value already on the stack.
    active: HashSet<(TypeId, Address)>,
    depth: usize,
    remaining: usize,
    in_pointee: bool,
}

impl<'a> ValueFormatter<'a>
{
    /// Use the built-in pretty-printer registry and the image's target byte order.
    pub fn new(
        types: &'a TypeTable,
        endian: RunTimeEndian,
        options: FormatOptions,
        read: &'a mut dyn FnMut(Address, usize) -> DebuggerResult<Vec<u8>>,
    ) -> Self
    {
        Self {
            types,
            endian,
            options,
            read,
            printers: PRINTERS,
            active: HashSet::new(),
            depth: 0,
            remaining: MAX_NODES,
            in_pointee: false,
        }
    }

    /// Replace the registry with an ordered list of printers for this formatter.
    #[must_use]
    pub fn with_printers(mut self, printers: &'a [&'a dyn PrettyPrinter]) -> Self
    {
        self.printers = printers;
        self
    }

    /// The image's type table.
    #[must_use]
    pub const fn types(&self) -> &TypeTable
    {
        self.types
    }

    /// Byte order for target scalars and addresses.
    #[must_use]
    pub const fn endian(&self) -> RunTimeEndian
    {
        self.endian
    }

    /// The limits in effect for this render.
    #[must_use]
    pub const fn options(&self) -> FormatOptions
    {
        self.options
    }

    /// Read a range on behalf of a renderer.
    ///
    /// # Errors
    /// Returns an unavailable reason for an oversized, overflowing or incomplete read,
    /// or an error reading the target.
    pub fn read_memory(&mut self, address: Address, len: usize) -> Result<Vec<u8>, Unavailable>
    {
        value_size(Some(len as u64))?;
        if len == 0 {
            return Ok(Vec::new());
        }
        if address.checked_add((len - 1) as u64).is_none() {
            return Err(Unavailable::Memory("memory range overflows address space".to_owned()));
        }
        let bytes = (self.read)(address, len).map_err(|error| Unavailable::Memory(error.to_string()))?;
        if bytes.len() != len {
            return Err(Unavailable::Memory(format!("expected {len} bytes, read {}", bytes.len())));
        }
        Ok(bytes)
    }

    /// Render a value whose bytes are already available.
    pub fn format(&mut self, value: ValueView<'_>) -> String
    {
        self.render(value.ty, value.address, Some(value.bytes))
    }

    /// Read and render a typed value at a runtime address.
    pub fn format_at(&mut self, ty: TypeId, address: Address) -> String
    {
        self.render(ty, Some(address), None)
    }

    fn render(&mut self, ty: TypeId, address: Option<Address>, bytes: Option<&[u8]>) -> String
    {
        if self.depth == 0 {
            self.remaining = MAX_NODES;
        }
        if self.remaining == 0 {
            return ELLIPSIS.to_owned();
        }
        self.remaining -= 1;
        if self.depth >= self.options.max_depth {
            return ELLIPSIS.to_owned();
        }
        let key = address.map(|address| (self.types.strip_aliases(ty), address));
        if key.is_some_and(|key| !self.active.insert(key)) {
            return "<cycle>".to_owned();
        }
        self.depth += 1;
        let text = self.render_value(ty, address, bytes);
        self.depth -= 1;
        if let Some(key) = key {
            self.active.remove(&key);
        }
        text
    }

    fn render_value(&mut self, mut id: TypeId, address: Option<Address>, bytes: Option<&[u8]>) -> String
    {
        let Some(mut ty) = self.types.get(id) else {
            return unavailable("type unknown");
        };
        let mut printer = self.printer(&ty);
        if printer.is_none() {
            let underlying = self.types.strip_aliases(id);
            if underlying != id
                && let Some(base) = self.types.get(underlying)
            {
                ty = base;
                id = underlying;
                printer = self.printer(&ty);
            }
        }
        // The generic array renderer can work with a prefix. Printers get a complete value.
        if printer.is_none()
            && let (None, Some(_), TypeKind::Array(array)) = (bytes, address, &ty.kind)
        {
            return self.format_sequence(array.element, &array.lengths, address, None);
        }
        let owned;
        let bytes = match (bytes, address) {
            (Some(bytes), _) => bytes,
            (None, Some(address)) => {
                owned = match value_size(ty.size).and_then(|size| self.read_memory(address, size)) {
                    Ok(bytes) => bytes,
                    Err(reason) => return unavailable(reason),
                };
                &owned
            }
            (None, None) => return unavailable("no storage"),
        };
        if let Some(size) = ty.size
            && size != bytes.len() as u64
        {
            return unavailable(format!("expected {size} value bytes, got {}", bytes.len()));
        }
        let value = ValueView { ty: id, bytes, address };
        if let Some(printer) = printer {
            return printer.format(value, self);
        }
        let name = ty.name.as_deref().unwrap_or("?");
        match &ty.kind {
            TypeKind::Base(encoding) => format_base(*encoding, name, self.endian, value.bytes),
            TypeKind::Struct(fields) if !ty.declaration => self.format_members(value, &fields.members, Some(name)),
            TypeKind::Tuple(fields) if !ty.declaration => self.format_members(value, &fields.members, None),
            TypeKind::Array(array) => self.format_sequence(array.element, &array.lengths, address, Some(bytes)),
            TypeKind::Pointer(pointer) => self.format_pointer(value, pointer),
            TypeKind::Enum(_) | TypeKind::Enumeration(_) => format!("<enum {name}>"),
            TypeKind::Unreadable(reason) => unavailable(reason),
            _ => format_raw(Ok(value.bytes)),
        }
    }

    fn printer(&self, ty: &Type) -> Option<&'a dyn PrettyPrinter>
    {
        self.printers.iter().copied().find(|printer| printer.matches(ty, self.types))
    }

    pub(super) fn format_members(&mut self, value: ValueView<'_>, members: &[Member], name: Option<&str>) -> String
    {
        // Rust describes tuple-struct fields with sequential names such as `__0`.
        let tuple_struct = name.is_some()
            && !members.is_empty()
            && members.iter().enumerate().all(|(index, member)| {
                member
                    .name
                    .as_deref()
                    .and_then(|name| name.strip_prefix("__"))
                    .and_then(|index| index.parse::<usize>().ok())
                    == Some(index)
            });
        let mut children = Vec::new();
        for member in members {
            if children.len() == self.options.max_elements || self.depth >= self.options.max_depth || self.remaining == 0 {
                children.push(ELLIPSIS.to_owned());
                break;
            }
            let text = match value.member(member, self.types) {
                Some(child) => self.format(child),
                None => {
                    self.remaining -= 1;
                    unavailable("member layout or bytes unknown")
                }
            };
            children.push(if name.is_some() && !tuple_struct {
                format!("{}: {text}", member.name.as_deref().unwrap_or("?"))
            } else {
                text
            });
        }
        match (name, children.as_slice()) {
            (Some(name), _) if tuple_struct => format!("{name}({})", children.join(", ")),
            (Some(name), []) => format!("{name} {{}}"),
            (Some(name), _) => format!("{name} {{ {} }}", children.join(", ")),
            (None, [child]) if members.len() == 1 && child != ELLIPSIS => format!("({child},)"),
            (None, _) => format!("({})", children.join(", ")),
        }
    }

    pub(super) fn format_sequence(
        &mut self,
        element: Option<TypeId>,
        lengths: &[Option<u64>],
        address: Option<Address>,
        bytes: Option<&[u8]>,
    ) -> String
    {
        let Some(element) = element else {
            return unavailable("array element type unknown");
        };
        let Some(size) = self.types.get(element).and_then(|ty| ty.size) else {
            return unavailable("array element size unknown");
        };
        self.format_dimensions(address, bytes, element, size, lengths)
    }

    fn format_dimensions(
        &mut self,
        address: Option<Address>,
        bytes: Option<&[u8]>,
        element: TypeId,
        size: u64,
        lengths: &[Option<u64>],
    ) -> String
    {
        let Some((&Some(len), inner)) = lengths.split_first() else {
            return unavailable("array length unknown");
        };
        let Some(stride) = inner.iter().try_fold(size, |stride, len| stride.checked_mul((*len)?)) else {
            return unavailable("invalid array layout");
        };
        let owned;
        let bytes = match (bytes, address) {
            (Some(bytes), _) => bytes,
            (None, Some(address)) => {
                let count = len.min(self.options.max_elements.min(self.remaining) as u64);
                let read_size = if self.depth >= self.options.max_depth {
                    Some(0)
                } else {
                    count.checked_mul(stride)
                };
                owned = match value_size(read_size).and_then(|size| self.read_memory(address, size)) {
                    Ok(bytes) => bytes,
                    Err(reason) => return unavailable(reason),
                };
                &owned
            }
            (None, None) => return unavailable("no storage"),
        };
        let value = ValueView {
            ty: element,
            bytes,
            address,
        };
        let mut children = Vec::new();
        for index in 0..len {
            if children.len() == self.options.max_elements || self.depth >= self.options.max_depth || self.remaining == 0 {
                children.push(ELLIPSIS.to_owned());
                break;
            }
            let child = index
                .checked_mul(stride)
                .and_then(|offset| value.slice(element, offset, stride));
            let text = match child {
                None => {
                    self.remaining -= 1;
                    unavailable("array element bytes missing")
                }
                Some(child) if inner.is_empty() => self.format(child),
                Some(child) => {
                    self.remaining -= 1;
                    self.depth += 1;
                    let text = self.format_dimensions(child.address, Some(child.bytes), element, size, inner);
                    self.depth -= 1;
                    text
                }
            };
            children.push(text);
        }
        format!("[{}]", children.join(", "))
    }

    fn format_pointer(&mut self, value: ValueView<'_>, pointer: &PointerType) -> String
    {
        if value.bytes.len() > 8 {
            return unavailable("invalid pointer width");
        }
        let Some(address) = unsigned(value.bytes, self.endian).and_then(|raw| u64::try_from(raw).ok()) else {
            return unavailable("invalid pointer bytes");
        };
        let text = format!("{address:#x}");
        if !matches!(pointer.kind, PointerKind::Reference { .. }) || self.in_pointee {
            return text;
        }
        self.in_pointee = true;
        let pointee = match pointer.pointee {
            _ if address == 0 => unavailable("null reference"),
            Some(ty) => self.format_at(ty, Address::new(address)),
            None => unavailable("pointee type unknown"),
        };
        self.in_pointee = false;
        format!("{text} -> {pointee}")
    }
}

/// Render bytes in memory order as hex, or preserve the unavailable reason.
#[must_use]
pub(crate) fn format_raw(bytes: Result<&[u8], &Unavailable>) -> String
{
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(reason) => return unavailable(reason),
    };
    let mut text = String::from("0x");
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

pub(super) fn unavailable(reason: impl Display) -> String
{
    format!("<unavailable: {reason}>")
}

pub(super) fn unsigned(bytes: &[u8], endian: RunTimeEndian) -> Option<u128>
{
    if bytes.is_empty() || bytes.len() > 16 {
        return None;
    }
    let step = |bits, &byte| (bits << 8) | u128::from(byte);
    Some(match endian {
        RunTimeEndian::Big => bytes.iter().fold(0, step),
        RunTimeEndian::Little => bytes.iter().rev().fold(0, step),
    })
}

fn format_base(encoding: BaseEncoding, name: &str, endian: RunTimeEndian, bytes: &[u8]) -> String
{
    if name == "()" && bytes.is_empty() {
        return "()".to_owned();
    }
    let Some(bits) = unsigned(bytes, endian) else {
        return format_raw(Ok(bytes));
    };
    match encoding {
        BaseEncoding::Signed | BaseEncoding::SignedChar => {
            let shift = 128 - 8 * bytes.len();
            (((bits << shift) as i128) >> shift).to_string()
        }
        BaseEncoding::Unsigned | BaseEncoding::UnsignedChar => bits.to_string(),
        BaseEncoding::Float => match bytes.len() {
            4 => format!("{:?}", f32::from_bits(bits as u32)),
            8 => format!("{:?}", f64::from_bits(bits as u64)),
            _ => format_raw(Ok(bytes)),
        },
        BaseEncoding::Boolean => match bits {
            0 => "false".to_owned(),
            1 => "true".to_owned(),
            _ => unavailable("invalid bool"),
        },
        BaseEncoding::Char => u32::try_from(bits)
            .ok()
            .and_then(char::from_u32)
            .map_or_else(|| unavailable("invalid char"), |ch| format!("{ch:?}")),
        BaseEncoding::Other(_) => format_raw(Ok(bytes)),
    }
}
