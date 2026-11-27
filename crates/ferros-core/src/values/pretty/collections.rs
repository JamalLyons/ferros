//! Render ring buffers, linked lists and binary heaps from their debug info.

use std::collections::HashSet;

use gimli::RunTimeEndian;

use super::{PrettyPrinter, field, raw_layout, usize_value, vec, vec_layout, word};
use crate::types::Address;
use crate::values::format::{ELLIPSIS, ValueFormatter, ValueView, unavailable};
use crate::values::types::{Type, TypeId, TypeKind, TypeTable};

const VEC_DEQUE_NAME: &str = "alloc::collections::vec_deque::VecDeque<";
const LINKED_LIST_NAME: &str = "alloc::collections::linked_list::LinkedList<";
const BINARY_HEAP_NAME: &str = "alloc::collections::binary_heap::BinaryHeap<";

#[derive(Clone, Copy)]
enum CollectionKind
{
    VecDeque,
    LinkedList,
    BinaryHeap,
}

impl CollectionKind
{
    fn of(ty: &Type) -> Option<Self>
    {
        let name = ty.name.as_deref()?;
        if name.starts_with(VEC_DEQUE_NAME) {
            Some(Self::VecDeque)
        } else if name.starts_with(LINKED_LIST_NAME) {
            Some(Self::LinkedList)
        } else if name.starts_with(BINARY_HEAP_NAME) {
            Some(Self::BinaryHeap)
        } else {
            None
        }
    }
}

pub(super) struct CollectionPrinter;

impl PrettyPrinter for CollectionPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        CollectionKind::of(ty).is_some()
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let endian = formatter.endian();
        let text = match types.get(value.ty).as_deref().and_then(CollectionKind::of) {
            Some(CollectionKind::VecDeque) => ring_layout(value, types, endian).map(|ring| ring.format(formatter)),
            Some(CollectionKind::LinkedList) => list_layout(value, types, endian).map(|list| list.format(formatter)),
            Some(CollectionKind::BinaryHeap) => format_heap(value, formatter),
            None => None,
        };
        text.unwrap_or_else(|| raw_layout(value, formatter))
    }
}

/// Render up to the element limit of `len` elements of type `ty`, asking `next` for each
/// element's address in logical order. Returns the text and whether it omits elements.
fn format_elements(
    len: u64,
    ty: TypeId,
    formatter: &mut ValueFormatter<'_>,
    mut next: impl FnMut(&mut ValueFormatter<'_>) -> Result<Address, String>,
) -> (String, bool)
{
    let mut children = Vec::new();
    let mut truncated = false;
    for _ in 0..len {
        if children.len() == formatter.options().max_elements || formatter.remaining_children() == 0 {
            children.push(ELLIPSIS.to_owned());
            truncated = true;
            break;
        }
        match next(formatter) {
            Ok(address) => children.push(formatter.format_at(ty, address)),
            Err(reason) => {
                children.push(unavailable(reason));
                truncated = true;
                break;
            }
        }
    }
    (format!("[{}]", children.join(", ")), truncated)
}

/// A `VecDeque` ring buffer: `len` elements starting at physical slot `head`, wrapping to
/// slot 0 at `capacity`.
struct Ring
{
    address: Address,
    head: u64,
    len: u64,
    capacity: u64,
    element: TypeId,
    stride: u64,
}

fn ring_layout(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<Ring>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    let element = layout.template_param("T")?.ty?;
    let stride = types.get(element)?.size?;
    let head = value.member(layout.member("head")?, types)?;
    // Recent releases wrap the physical head index in a newtype.
    let head = usize_value(field(head, "__0", types).unwrap_or(head), types, endian)?;
    let len = usize_value(value.member(layout.member("len")?, types)?, types, endian)?;
    let (address, capacity) = vec::raw_buffer(value.member(layout.member("buf")?, types)?, stride, types, endian)?;
    Some(Ring {
        address,
        head,
        len,
        capacity,
        element,
        stride,
    })
}

impl Ring
{
    fn format(&self, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Self {
            address,
            head,
            len,
            capacity,
            element,
            stride,
        } = *self;
        if len > capacity {
            return unavailable(format!("VecDeque length {len} exceeds capacity {capacity}"));
        }
        if len > 0 && head >= capacity {
            return unavailable(format!("VecDeque head {head} is outside capacity {capacity}"));
        }
        if len > 0 && address == Address::ZERO {
            return unavailable("null VecDeque buffer pointer");
        }
        let mut index = 0;
        let (elements, _) = format_elements(len, element, formatter, |_| {
            // head < capacity and index < len <= capacity: the slot wraps at most once.
            let tail_room = capacity - head;
            let slot = if index < tail_room { head + index } else { index - tail_room };
            index += 1;
            slot.checked_mul(stride)
                .and_then(|offset| address.checked_add(offset))
                .ok_or_else(|| "VecDeque element address overflows".to_owned())
        });
        format!("{elements} (len: {len}, capacity: {capacity})")
    }
}

/// A `LinkedList`: `len` nodes linked through `next` from `head`.
struct List
{
    head: Address,
    len: u64,
    next_offset: u64,
    next_size: usize,
    element: TypeId,
    element_offset: u64,
}

fn list_layout(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<List>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    let head = value.member(layout.member("head")?, types)?;
    let node_ty = link_target(head.ty, types)?;
    let head = Address::new(word(head.bytes, endian)?);
    let len = usize_value(value.member(layout.member("len")?, types)?, types, endian)?;
    let node = types.get(node_ty)?;
    let TypeKind::Struct(node) = &node.kind else {
        return None;
    };
    let next = node.member("next")?;
    let next_ty = next.ty?;
    if next.bit_size.is_some() || link_target(next_ty, types)? != node_ty {
        return None;
    }
    let element = node.member("element")?;
    if element.bit_size.is_some() {
        return None;
    }
    Some(List {
        head,
        len,
        next_offset: next.offset?,
        next_size: usize::try_from(types.get(types.strip_aliases(next_ty))?.size?).ok()?,
        element: element.ty?,
        element_offset: element.offset?,
    })
}

/// The node type of an `Option<NonNull<Node>>` link that the null pointer encodes as
/// `None`, so the link is one pointer word.
fn link_target(option: TypeId, types: &TypeTable) -> Option<TypeId>
{
    let option = types.get(types.strip_aliases(option))?;
    let TypeKind::Enum(layout) = &option.kind else {
        return None;
    };
    let [payload] = layout.variant("Some")?.members.as_slice() else {
        return None;
    };
    let payload = only_member(payload.ty?, "__0", types)?;
    let pointer = only_member(payload, "pointer", types)?;
    let pointer = types.get(types.strip_aliases(pointer))?;
    let TypeKind::Pointer(target) = &pointer.kind else {
        return None;
    };
    let size = pointer.size?;
    if option.size? != size || !(1..=8).contains(&size) {
        return None;
    }
    Some(types.strip_aliases(target.pointee?))
}

/// The type of `name`, when it is a struct member at offset zero.
fn only_member(container: TypeId, name: &str, types: &TypeTable) -> Option<TypeId>
{
    let container = types.get(types.strip_aliases(container))?;
    let TypeKind::Struct(layout) = &container.kind else {
        return None;
    };
    let member = layout.member(name)?;
    (member.offset == Some(0) && member.bit_size.is_none()).then_some(member.ty?)
}

impl List
{
    fn format(&self, formatter: &mut ValueFormatter<'_>) -> String
    {
        let len = self.len;
        let mut node = self.head;
        let mut seen = HashSet::new();
        let (elements, truncated) = format_elements(len, self.element, formatter, |formatter| {
            if node == Address::ZERO {
                return Err("LinkedList has fewer nodes than its length".to_owned());
            }
            // The walk is bounded by len and the element limit, but a corrupt list could
            // still revisit a node before either.
            if !seen.insert(node) {
                return Err("repeated LinkedList node".to_owned());
            }
            let overflow = || "LinkedList node address overflows".to_owned();
            let element = node.checked_add(self.element_offset).ok_or_else(overflow)?;
            let link = node.checked_add(self.next_offset).ok_or_else(overflow)?;
            let bytes = formatter.read_memory(link, self.next_size).map_err(|err| err.to_string())?;
            node = Address::new(word(&bytes, formatter.endian()).ok_or("invalid LinkedList link")?);
            Ok(element)
        });
        if truncated {
            format!("{elements} (len: {len})")
        } else {
            elements
        }
    }
}

/// A `BinaryHeap` is a `Vec` in heap order. Show its elements in storage order.
fn format_heap(value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> Option<String>
{
    let types = formatter.types();
    let data = vec_layout(field(value, "data", types)?, types, formatter.endian())?;
    Some(vec::format_contents(&data, "heap", formatter))
}
