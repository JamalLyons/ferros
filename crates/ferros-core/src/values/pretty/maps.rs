//! Preview initialized entries in Rust's hash tables and B-trees.

use std::collections::HashSet;

use gimli::RunTimeEndian;

use super::{PrettyPrinter, field, raw_layout, thin_pointer, usize_value, word};
use crate::types::Address;
use crate::values::format::{ELLIPSIS, ValueFormatter, ValueView, unavailable, unsigned};
use crate::values::types::{BaseEncoding, Type, TypeId, TypeKind, TypeTable};

const STD_MAP: &str = "std::collections::hash::map::HashMap<";
const STD_SET: &str = "std::collections::hash::set::HashSet<";
const HASH_MAP: &str = "hashbrown::map::HashMap<";
const HASH_SET: &str = "hashbrown::set::HashSet<";
const TREE_MAP: &str = "alloc::collections::btree::map::BTreeMap<";
const TREE_SET: &str = "alloc::collections::btree::set::BTreeSet<";
const MAX_CONTROL_BYTES: u64 = 64 * 1024;
const CONTROL_CHUNK: u64 = 4096;
// A valid tree with a 64-bit element count cannot have this many levels.
const MAX_TREE_HEIGHT: u64 = 63;

#[derive(Clone, Copy)]
enum MapKind
{
    HashMap,
    HashSet,
    BTreeMap,
    BTreeSet,
}

impl MapKind
{
    fn of(ty: &Type) -> Option<Self>
    {
        let name = ty.name.as_deref()?;
        if name.starts_with(STD_MAP) || name.starts_with(HASH_MAP) {
            Some(Self::HashMap)
        } else if name.starts_with(STD_SET) || name.starts_with(HASH_SET) {
            Some(Self::HashSet)
        } else if name.starts_with(TREE_MAP) {
            Some(Self::BTreeMap)
        } else if name.starts_with(TREE_SET) {
            Some(Self::BTreeSet)
        } else {
            None
        }
    }
}

pub(super) struct MapPrinter;

impl PrettyPrinter for MapPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        MapKind::of(ty).is_some()
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let limit = formatter.options().max_elements.min(formatter.remaining_children()) as u64;
        let Some(kind) = formatter.types().get(value.ty).and_then(|ty| MapKind::of(&ty)) else {
            return raw_layout(value, formatter);
        };
        let preview = match kind {
            MapKind::BTreeMap => tree_preview(value, limit, formatter),
            // A set is a map whose zero-sized values are not shown.
            MapKind::BTreeSet => field(value, "map", formatter.types()).and_then(|map| {
                let (len, entries) = tree_preview(map, limit, formatter)?;
                let keys = entries.map(|entries| entries.into_iter().map(|entry| Entry { value: None, ..entry }).collect());
                Some((len, keys))
            }),
            kind => hash_preview(value, kind, limit, formatter),
        };
        let Some((len, entries)) = preview else {
            return raw_layout(value, formatter);
        };
        let entries = match entries {
            Ok(entries) => entries,
            Err(reason) => return unavailable(reason),
        };
        let mut children = Vec::new();
        for entry in entries {
            if formatter.remaining_children() == 0 {
                break;
            }
            let key = formatter.format_at(entry.key.ty, entry.key.address);
            children.push(match entry.value {
                Some(value) => format!("{key}: {}", formatter.format_at(value.ty, value.address)),
                None => key,
            });
        }
        if (children.len() as u64) < len {
            children.push(ELLIPSIS.to_owned());
            format!("{{{}}} (len: {len})", children.join(", "))
        } else {
            format!("{{{}}}", children.join(", "))
        }
    }
}

struct Location
{
    ty: TypeId,
    address: Address,
}

struct Entry
{
    key: Location,
    value: Option<Location>,
}

#[derive(Clone, Copy)]
struct Slot
{
    ty: TypeId,
    offset: u64,
    size: u64,
}

impl Slot
{
    fn named(container: &Type, name: &str, types: &TypeTable) -> Option<Self>
    {
        let (TypeKind::Struct(fields) | TypeKind::Tuple(fields)) = &container.kind else {
            return None;
        };
        let member = fields.member(name)?;
        let ty = member.ty?;
        let size = types.get(ty)?.size?;
        let offset = member.offset?;
        if member.bit_size.is_some() || offset.checked_add(size)? > container.size? {
            return None;
        }
        Some(Self { ty, offset, size })
    }

    fn at(self, base: Address) -> Result<Location, String>
    {
        Ok(Location {
            ty: self.ty,
            address: base.checked_add(self.offset).ok_or("map slot address overflows")?,
        })
    }

    fn read_word(self, base: Address, formatter: &mut ValueFormatter<'_>) -> Result<u64, String>
    {
        let bytes = formatter
            .read_memory(self.at(base)?.address, self.size as usize)
            .map_err(|err| err.to_string())?;
        word(&bytes, formatter.endian()).ok_or_else(|| "invalid map word".to_owned())
    }
}

type Preview = (u64, Result<Vec<Entry>, String>);

struct HashTable
{
    control: Address,
    buckets: u64,
    stride: u64,
    key: Slot,
    value: Option<Slot>,
}

fn hash_preview(value: ValueView<'_>, kind: MapKind, limit: u64, formatter: &mut ValueFormatter<'_>) -> Option<Preview>
{
    let types = formatter.types();
    let endian = formatter.endian();
    let ty = types.get(value.ty)?;
    let name = ty.name.as_deref()?;
    let base = if name.starts_with(STD_MAP) || name.starts_with(STD_SET) {
        field(value, "base", types)?
    } else {
        value
    };
    let map = if matches!(kind, MapKind::HashSet) {
        field(base, "map", types)?
    } else {
        base
    };
    let table = field(map, "table", types)?;
    let raw = field(table, "table", types)?;
    let len = usize_value(field(raw, "items", types)?, types, endian)?;
    if len == 0 || limit == 0 {
        return Some((len, Ok(Vec::new())));
    }
    let table_type = resolved(types, table.ty)?;
    let TypeKind::Struct(fields) = &table_type.kind else {
        return None;
    };
    let tuple = resolved(types, fields.template_param("T")?.ty?)?;
    let key = Slot::named(&tuple, "__0", types)?;
    let value = if matches!(kind, MapKind::HashSet) {
        None
    } else {
        Some(Slot::named(&tuple, "__1", types)?)
    };
    let mask = usize_value(field(raw, "bucket_mask", types)?, types, endian)?;
    let control = field(field(raw, "ctrl", types)?, "pointer", types)?;
    let (control, byte) = thin_pointer(control, types, endian)?;
    let byte = resolved(types, byte)?;
    if byte.name.as_deref() != Some("u8") || byte.size != Some(1) {
        return None;
    }
    let Some(buckets) = mask.checked_add(1).filter(|n| n.is_power_of_two() && *n > len) else {
        return Some((len, Err("invalid hash table bucket count".to_owned())));
    };
    if control == Address::ZERO {
        return Some((len, Err("null hash table control pointer".to_owned())));
    }
    let table = HashTable {
        control,
        buckets,
        stride: tuple.size?,
        key,
        value,
    };
    Some((len, table.entries(len.min(limit), formatter)))
}

impl HashTable
{
    fn entries(&self, limit: u64, formatter: &mut ValueFormatter<'_>) -> Result<Vec<Entry>, String>
    {
        let mut entries = Vec::new();
        // Sparse tables can have far more buckets than entries. Limit work even when
        // few occupied buckets appear in the scanned prefix.
        let scan_len = self.buckets.min(MAX_CONTROL_BYTES);
        let mut offset = 0;
        while offset < scan_len {
            let size = (scan_len - offset).min(CONTROL_CHUNK);
            let address = self.control.checked_add(offset).ok_or("hash control address overflows")?;
            let controls = formatter.read_memory(address, size as usize).map_err(|err| err.to_string())?;
            for (index, tag) in controls.into_iter().enumerate() {
                if tag & 0x80 != 0 {
                    continue;
                }
                // Occupied buckets run backwards immediately before the control bytes.
                let bucket = offset + index as u64;
                let distance = (bucket + 1).checked_mul(self.stride).ok_or("hash bucket offset overflows")?;
                let address = self.control.checked_sub(distance).ok_or("hash bucket address underflows")?;
                entries.push(Entry {
                    key: self.key.at(address)?,
                    value: self.value.map(|slot| slot.at(address)).transpose()?,
                });
                if entries.len() as u64 == limit {
                    return Ok(entries);
                }
            }
            offset += size;
        }
        if scan_len == self.buckets {
            return Err("hash table has fewer occupied buckets than its count".to_owned());
        }
        Ok(entries)
    }
}

struct Tree
{
    len: Slot,
    keys: Slots,
    values: Slots,
    edges: Option<(Slots, Slot)>,
}

struct Slots
{
    offset: u64,
    stride: u64,
    count: u64,
    ty: TypeId,
}

impl Slots
{
    fn of(slot: Slot, types: &TypeTable) -> Option<Self>
    {
        let array = resolved(types, slot.ty)?;
        let TypeKind::Array(array) = &array.kind else {
            return None;
        };
        let [Some(count)] = array.lengths.as_slice() else {
            return None;
        };
        let wrapper = resolved(types, array.element?)?;
        let TypeKind::Union(fields) = &wrapper.kind else {
            return None;
        };
        if !wrapper.name.as_deref()?.starts_with("core::mem::maybe_uninit::MaybeUninit<") {
            return None;
        }
        // MaybeUninit has the size and alignment of its type parameter.
        let ty = fields.template_param("T")?.ty?;
        let stride = types.get(ty)?.size?;
        if wrapper.size != Some(stride) || stride.checked_mul(*count)? != slot.size {
            return None;
        }
        Some(Self {
            offset: slot.offset,
            stride,
            count: *count,
            ty,
        })
    }

    fn at(&self, base: Address, index: u64) -> Result<Location, String>
    {
        let offset = index
            .checked_mul(self.stride)
            .and_then(|offset| offset.checked_add(self.offset))
            .ok_or("B-tree slot offset overflows")?;
        Ok(Location {
            ty: self.ty,
            address: base.checked_add(offset).ok_or("B-tree slot address overflows")?,
        })
    }
}

fn resolved(types: &TypeTable, ty: TypeId) -> Option<std::sync::Arc<Type>>
{
    types.get(types.strip_aliases(ty))
}

fn some_payload<'a>(option: ValueView<'a>, types: &TypeTable, endian: RunTimeEndian) -> Option<ValueView<'a>>
{
    let ty = resolved(types, option.ty)?;
    let TypeKind::Enum(layout) = &ty.kind else {
        return None;
    };
    let tag = option.member(layout.discriminant.as_ref()?, types)?;
    let variant = layout.variant_for(unsigned(tag.bytes, endian)?)?;
    let [payload] = variant.members.as_slice() else {
        return None;
    };
    if variant.name.as_deref() != Some("Some") {
        return None;
    }
    field(option.member(payload, types)?, "__0", types)
}

fn pointer_slot(non_null: TypeId, types: &TypeTable) -> Option<(Slot, TypeId)>
{
    let wrapper = resolved(types, non_null)?;
    let slot = Slot::named(&wrapper, "pointer", types)?;
    let ty = resolved(types, slot.ty)?;
    let TypeKind::Pointer(pointer) = &ty.kind else {
        return None;
    };
    (1..=8).contains(&slot.size).then_some((slot, pointer.pointee?))
}

fn tree_layout(leaf_ty: TypeId, height: u64, types: &TypeTable) -> Option<Tree>
{
    let leaf = resolved(types, leaf_ty)?;
    let len = Slot::named(&leaf, "len", types)?;
    if len.size != 2 || !matches!(resolved(types, len.ty)?.kind, TypeKind::Base(BaseEncoding::Unsigned)) {
        return None;
    }
    let keys = Slots::of(Slot::named(&leaf, "keys", types)?, types)?;
    let values = Slots::of(Slot::named(&leaf, "vals", types)?, types)?;
    if keys.count == 0 || keys.count != values.count {
        return None;
    }
    let edges = if height == 0 {
        None
    } else {
        // A leaf's parent type identifies its corresponding internal node in this unit.
        let parent = resolved(types, Slot::named(&leaf, "parent", types)?.ty)?;
        let TypeKind::Enum(layout) = &parent.kind else {
            return None;
        };
        let (_, internal_ty) = pointer_slot(layout.template_param("T")?.ty?, types)?;
        let internal = resolved(types, internal_ty)?;
        let data = Slot::named(&internal, "data", types)?;
        let edges = Slots::of(Slot::named(&internal, "edges", types)?, types)?;
        let (pointer, child) = pointer_slot(edges.ty, types)?;
        let leaf = types.strip_aliases(leaf_ty);
        if data.offset != 0
            || types.strip_aliases(data.ty) != leaf
            || types.strip_aliases(child) != leaf
            || edges.count != keys.count.checked_add(1)?
        {
            return None;
        }
        Some((edges, pointer))
    };
    Some(Tree {
        len,
        keys,
        values,
        edges,
    })
}

fn tree_preview(value: ValueView<'_>, limit: u64, formatter: &mut ValueFormatter<'_>) -> Option<Preview>
{
    let types = formatter.types();
    let endian = formatter.endian();
    let len = usize_value(field(value, "length", types)?, types, endian)?;
    if len == 0 || limit == 0 {
        return Some((len, Ok(Vec::new())));
    }
    let root = some_payload(field(value, "root", types)?, types, endian)?;
    let height = usize_value(field(root, "height", types)?, types, endian)?;
    if height > MAX_TREE_HEIGHT {
        return Some((len, Err("invalid B-tree height".to_owned())));
    }
    let pointer = field(field(root, "node", types)?, "pointer", types)?;
    let (address, leaf_ty) = thin_pointer(pointer, types, endian)?;
    let tree = tree_layout(leaf_ty, height, types)?;
    let mut walk = TreeWalk {
        seen: HashSet::new(),
        entries: Vec::new(),
        limit: len.min(limit),
    };
    let entries = tree.visit(address, height, &mut walk, formatter).and_then(|()| {
        if walk.entries.len() as u64 != walk.limit {
            return Err("B-tree has fewer entries than its count".to_owned());
        }
        Ok(walk.entries)
    });
    Some((len, entries))
}

struct TreeWalk
{
    seen: HashSet<Address>,
    entries: Vec<Entry>,
    limit: u64,
}

impl Tree
{
    fn visit(
        &self,
        node: Address,
        height: u64,
        walk: &mut TreeWalk,
        formatter: &mut ValueFormatter<'_>,
    ) -> Result<(), String>
    {
        if node == Address::ZERO || !walk.seen.insert(node) {
            return Err("null or repeated B-tree node".to_owned());
        }
        let len = self.len.read_word(node, formatter)?;
        if len == 0 || len > self.keys.count {
            return Err("invalid B-tree node length".to_owned());
        }
        for index in 0..=len {
            if height > 0 {
                let (edges, pointer) = self.edges.as_ref().ok_or("missing B-tree edge layout")?;
                let edge = edges.at(node, index)?;
                let child = Address::new(pointer.read_word(edge.address, formatter)?);
                self.visit(child, height - 1, walk, formatter)?;
            }
            if walk.entries.len() as u64 == walk.limit {
                break;
            }
            if index < len {
                walk.entries.push(Entry {
                    key: self.keys.at(node, index)?,
                    value: Some(self.values.at(node, index)?),
                });
                if walk.entries.len() as u64 == walk.limit {
                    break;
                }
            }
        }
        Ok(())
    }
}
