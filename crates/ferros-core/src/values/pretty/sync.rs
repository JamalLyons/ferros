//! Render Rust locks, atomics, lazily set cells, `UnsafeCell` and `Pin` as the value they
//! guard, with the state the standard library keeps beside it.
//!
//! Lock state comes from the standard library's own lock words. A `Mutex` or `RwLock`
//! built on a 32-bit wait word (`futex`), and an `RwLock` built on a tagged queue pointer,
//! show whether they are locked. Where the standard library keeps the operating system's
//! mutex behind a lazily allocated box (`pal`), the `Mutex` is `unlocked` while the box is
//! unallocated (it has never been locked). Once it is allocated, its state is in the
//! system's private layout, which the session's
//! [`TargetRuntime::system_mutex_state`] reads; without a runtime that knows the layout,
//! no lock state shows. Poisoning is the standard library's own flag and always shows.
//!
//! A lock guard shows the value it guards, reached through the guard's pointer to its
//! lock or to the value.

use gimli::RunTimeEndian;

use super::dyn_trait::DynMetadata;
use super::{PrettyPrinter, fat_pointer, field, raw_layout, thin_pointer, variant_for_tag, variant_payload, vec, word};
use crate::runtime::{LockState, TargetRuntime};
use crate::types::Address;
use crate::values::format::{ValueFormatter, ValueView, unavailable, unsigned};
use crate::values::types::{BaseEncoding, Member, Type, TypeId, TypeKind, TypeTable};

const UNSAFE_CELL_NAME: &str = "core::cell::UnsafeCell<";
const ATOMIC_NAME: &str = "core::sync::atomic::Atomic";
const ATOMIC_BOOL_NAMES: &[&str] = &["core::sync::atomic::Atomic<bool>", "core::sync::atomic::AtomicBool"];
const ONCE_CELL_NAME: &str = "core::cell::once::OnceCell<";
const ONCE_LOCK_NAME: &str = "std::sync::once_lock::OnceLock<";
const PIN_NAME: &str = "core::pin::Pin<";
const MUTEX_NAMES: &[&str] = &[
    "std::sync::poison::mutex::Mutex<",
    "std::sync::nonpoison::mutex::Mutex<",
    "std::sync::mutex::Mutex<",
];
const RW_LOCK_NAMES: &[&str] = &[
    "std::sync::poison::rwlock::RwLock<",
    "std::sync::nonpoison::rwlock::RwLock<",
    "std::sync::rwlock::RwLock<",
];
const MUTEX_GUARD_NAMES: &[&str] = &[
    "std::sync::poison::mutex::MutexGuard<",
    "std::sync::nonpoison::mutex::MutexGuard<",
    "std::sync::mutex::MutexGuard<",
];
const READ_GUARD_NAMES: &[&str] = &[
    "std::sync::poison::rwlock::RwLockReadGuard<",
    "std::sync::nonpoison::rwlock::RwLockReadGuard<",
    "std::sync::rwlock::RwLockReadGuard<",
];
const WRITE_GUARD_NAMES: &[&str] = &[
    "std::sync::poison::rwlock::RwLockWriteGuard<",
    "std::sync::nonpoison::rwlock::RwLockWriteGuard<",
    "std::sync::rwlock::RwLockWriteGuard<",
];

// A `Once` keeps its state in the low two bits of its state word.
const ONCE_STATE_MASK: u64 = 0b11;
const ONCE_COMPLETE: u64 = 0;
const ONCE_RUNNING: u64 = 1;
const ONCE_POISONED: u64 = 2;

// A 32-bit wait-word `RwLock` counts readers in its low 30 bits; all ones is write-locked.
const FUTEX_RW_MASK: u64 = (1 << 30) - 1;
// A queue `RwLock` tags its state pointer. Without queued waiters the bits above the tags
// count readers in units of `QUEUE_SINGLE`.
const QUEUE_LOCKED: u64 = 1;
const QUEUE_QUEUED: u64 = 1 << 1;
const QUEUE_SINGLE: u64 = 1 << 4;
// Larger than any system mutex object, so a corrupt size cannot cause a large read.
const MAX_SYSTEM_MUTEX_BYTES: u64 = 256;

fn has_prefix(ty: &Type, prefix: &str) -> bool
{
    ty.name.as_deref().is_some_and(|name| name.starts_with(prefix))
}

fn has_any_prefix(ty: &Type, prefixes: &[&str]) -> bool
{
    prefixes.iter().any(|prefix| has_prefix(ty, prefix))
}

fn type_named(value: ValueView<'_>, types: &TypeTable, prefix: &str) -> bool
{
    types
        .get(types.strip_aliases(value.ty))
        .is_some_and(|ty| has_prefix(&ty, prefix))
}

/// The `value` an `UnsafeCell` wraps.
fn unsafe_cell_value<'a>(cell: ValueView<'a>, types: &TypeTable) -> Option<ValueView<'a>>
{
    let ty = types.get(types.strip_aliases(cell.ty))?;
    cell.member(unsafe_cell_member(&ty)?, types)
}

fn unsafe_cell_member(ty: &Type) -> Option<&Member>
{
    if !has_prefix(ty, UNSAFE_CELL_NAME) {
        return None;
    }
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    let [value] = layout.members.as_slice() else {
        return None;
    };
    (value.name.as_deref() == Some("value") && value.offset == Some(0) && value.bit_size.is_none()).then_some(value)
}

/// The stored primitive of an atomic: the sole field's `UnsafeCell`, without the
/// alignment newtype recent standard libraries wrap it in.
fn atomic_storage<'a>(atomic: ValueView<'a>, types: &TypeTable) -> Option<ValueView<'a>>
{
    let ty = types.get(types.strip_aliases(atomic.ty))?;
    if !has_prefix(&ty, ATOMIC_NAME) {
        return None;
    }
    let TypeKind::Struct(layout) = &ty.kind else {
        return None;
    };
    let [member] = layout.members.as_slice() else {
        return None;
    };
    let stored = unsafe_cell_value(atomic.member(member, types)?, types)?;
    let aligned = types.get(types.strip_aliases(stored.ty))?;
    if let TypeKind::Struct(newtype) = &aligned.kind
        && let [inner] = newtype.members.as_slice()
        && inner.name.as_deref() == Some("__0")
    {
        return stored.member(inner, types);
    }
    Some(stored)
}

fn atomic_word(atomic: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<u64>
{
    let storage = atomic_storage(atomic, types)?;
    let ty = types.get(types.strip_aliases(storage.ty))?;
    if !matches!(
        ty.kind,
        TypeKind::Base(BaseEncoding::Unsigned | BaseEncoding::UnsignedChar | BaseEncoding::Boolean) | TypeKind::Pointer(_)
    ) {
        return None;
    }
    word(storage.bytes, endian)
}

fn atomic_bool(atomic: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<bool>
{
    match atomic_word(atomic, types, endian)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

pub(super) struct UnsafeCellPrinter;

impl PrettyPrinter for UnsafeCellPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        has_prefix(ty, UNSAFE_CELL_NAME)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        match unsafe_cell_value(value, formatter.types()) {
            Some(inner) => formatter.format(inner),
            None => raw_layout(value, formatter),
        }
    }
}

pub(super) struct AtomicPrinter;

impl PrettyPrinter for AtomicPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        has_prefix(ty, ATOMIC_NAME)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let Some(storage) = atomic_storage(value, types) else {
            return raw_layout(value, formatter);
        };
        let is_bool = types
            .get(types.strip_aliases(value.ty))
            .is_some_and(|ty| ty.name.as_deref().is_some_and(|name| ATOMIC_BOOL_NAMES.contains(&name)));
        if !is_bool {
            return formatter.format(storage);
        }
        if storage.bytes.len() != 1 {
            return raw_layout(value, formatter);
        }
        match unsigned(storage.bytes, formatter.endian()) {
            Some(0) => "false".to_owned(),
            Some(1) => "true".to_owned(),
            _ => unavailable("invalid bool"),
        }
    }
}

pub(super) struct PinPrinter;

impl PrettyPrinter for PinPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        has_prefix(ty, PIN_NAME)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        match field(value, "pointer", formatter.types()) {
            Some(pointer) => formatter.format(pointer),
            None => raw_layout(value, formatter),
        }
    }
}

pub(super) struct OnceCellPrinter;

impl PrettyPrinter for OnceCellPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        has_prefix(ty, ONCE_CELL_NAME) || has_prefix(ty, ONCE_LOCK_NAME)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let state = if type_named(value, types, ONCE_CELL_NAME) {
            once_cell_state(value, types, formatter.endian()).map(|state| ("OnceCell", state))
        } else {
            once_lock_state(value, types, formatter.endian()).map(|state| ("OnceLock", state))
        };
        let Some((name, state)) = state else {
            return raw_layout(value, formatter);
        };
        let shown = match state {
            Once::Set(inner) => formatter.format(inner),
            Once::Uninit => "<uninit>".to_owned(),
            Once::Running => "<initializing>".to_owned(),
            Once::Poisoned => "<poisoned>".to_owned(),
        };
        format!("{name}({shown})")
    }
}

enum Once<'a>
{
    Set(ValueView<'a>),
    Uninit,
    Running,
    Poisoned,
}

/// `OnceCell { inner: UnsafeCell<Option<T>> }`.
fn once_cell_state<'a>(value: ValueView<'a>, types: &TypeTable, endian: RunTimeEndian) -> Option<Once<'a>>
{
    let option = unsafe_cell_value(field(value, "inner", types)?, types)?;
    Some(option_payload(option, types, endian)?.map_or(Once::Uninit, Once::Set))
}

/// `OnceLock { once: Once { inner: <state word> }, value: UnsafeCell<MaybeUninit<T>> }`.
fn once_lock_state<'a>(value: ValueView<'a>, types: &TypeTable, endian: RunTimeEndian) -> Option<Once<'a>>
{
    let once = field(field(value, "once", types)?, "inner", types)?;
    let state = field(once, "state_and_queue", types).or_else(|| field(once, "state_and_queued", types))?;
    Some(match atomic_word(state, types, endian)? & ONCE_STATE_MASK {
        ONCE_COMPLETE => {
            let uninit = unsafe_cell_value(field(value, "value", types)?, types)?;
            let manually_drop = union_field(uninit, "value", types)?;
            Once::Set(field(manually_drop, "value", types)?)
        }
        ONCE_RUNNING => Once::Running,
        ONCE_POISONED => Once::Poisoned,
        _ => Once::Uninit,
    })
}

fn union_field<'a>(value: ValueView<'a>, name: &str, types: &TypeTable) -> Option<ValueView<'a>>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    let TypeKind::Union(layout) = &ty.kind else {
        return None;
    };
    value.member(layout.member(name)?, types)
}

/// `Some(payload)` or `None` of a `core::option::Option`; `None` when the value is not one
/// or its variant cannot be read.
fn option_payload<'a>(value: ValueView<'a>, types: &TypeTable, endian: RunTimeEndian) -> Option<Option<ValueView<'a>>>
{
    let ty = types.get(types.strip_aliases(value.ty))?;
    if !has_prefix(&ty, "core::option::Option<") {
        return None;
    }
    let TypeKind::Enum(layout) = &ty.kind else {
        return None;
    };
    let tag = layout
        .discriminant
        .as_ref()
        .and_then(|discriminant| value.member(discriminant, types))
        .map(|tag| tag.bytes);
    let variant = variant_for_tag(layout, tag, endian).ok()?;
    match variant.name.as_deref()? {
        "None" => Some(None),
        "Some" => {
            let payload = variant_payload(variant, ty.name.as_deref(), types)?;
            let [inner] = payload.fields() else {
                return None;
            };
            Some(Some(value.member(payload.member, types)?.member(inner, types)?))
        }
        _ => None,
    }
}

/// Renders `Mutex` and `RwLock`. The session's printer list holds one with its
/// [`TargetRuntime`]; the default registry's has none.
pub(crate) struct LockPrinter<'a>
{
    runtime: Option<&'a dyn TargetRuntime>,
}

impl<'a> LockPrinter<'a>
{
    pub(super) const WITHOUT_RUNTIME: LockPrinter<'static> = LockPrinter { runtime: None };

    /// A printer that asks `runtime` about system mutexes.
    pub(crate) fn with_runtime(runtime: &'a dyn TargetRuntime) -> Self
    {
        Self { runtime: Some(runtime) }
    }
}

impl PrettyPrinter for LockPrinter<'_>
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        has_any_prefix(ty, MUTEX_NAMES) || has_any_prefix(ty, RW_LOCK_NAMES)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let types = formatter.types();
        let endian = formatter.endian();
        let is_mutex = types
            .get(types.strip_aliases(value.ty))
            .is_some_and(|ty| has_any_prefix(&ty, MUTEX_NAMES));
        let Some((inner, data)) = field(value, "inner", types).zip(field(value, "data", types)) else {
            return raw_layout(value, formatter);
        };
        if unsafe_cell_value(data, types).is_none() {
            return raw_layout(value, formatter);
        }
        let Some(poisoned) = poison_flag_set(field(value, "poison", types), types, endian) else {
            return raw_layout(value, formatter);
        };
        let lock = if is_mutex {
            self.mutex_state(inner, formatter)
        } else {
            rw_lock_state(inner, types, endian)
        };
        // A poisoned lock is usually free again: its holder panicked and released it.
        let mut states: Vec<String> = lock.into_iter().filter(|state| !(poisoned && state == "unlocked")).collect();
        if poisoned {
            states.push("poisoned".to_owned());
        }
        let name = if is_mutex { "Mutex" } else { "RwLock" };
        let data = formatter.format(data);
        if states.is_empty() {
            format!("{name} {data}")
        } else {
            format!("{name}({}) {data}", states.join(", "))
        }
    }
}

/// Whether a lock's `poison` flag is set; `None` when it cannot be read.
fn poison_flag_set(flag: Option<ValueView<'_>>, types: &TypeTable, endian: RunTimeEndian) -> Option<bool>
{
    // A lock without a poison flag (the `nonpoison` locks), or with a flag that has no
    // `failed` field (a standard library built to abort on panic), is never poisoned.
    let Some(flag) = flag else {
        return Some(false);
    };
    match field(flag, "failed", types) {
        None => Some(false),
        Some(failed) => atomic_bool(failed, types, endian),
    }
}

fn has_member(value: ValueView<'_>, name: &str, types: &TypeTable) -> bool
{
    types
        .get(types.strip_aliases(value.ty))
        .is_some_and(|ty| matches!(&ty.kind, TypeKind::Struct(layout) if layout.member(name).is_some()))
}

impl LockPrinter<'_>
{
    fn mutex_state(&self, inner: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> Option<String>
    {
        let types = formatter.types();
        let endian = formatter.endian();
        if let Some(futex) = field(inner, "futex", types) {
            return match atomic_word(futex, types, endian)? {
                0 => Some("unlocked".to_owned()),
                1 | 2 => Some("locked".to_owned()),
                _ => None,
            };
        }
        // The boxed system mutex is allocated the first time the lock is taken.
        let boxed = field(field(inner, "pal", types)?, "ptr", types)?;
        let address = atomic_word(boxed, types, endian)?;
        if address == 0 {
            return Some("unlocked".to_owned());
        }
        let runtime = self.runtime?;
        let storage = atomic_storage(boxed, types)?;
        let TypeKind::Pointer(pointer) = &types.get(types.strip_aliases(storage.ty))?.kind else {
            return None;
        };
        let size = types.get(pointer.pointee?)?.size?;
        if size > MAX_SYSTEM_MUTEX_BYTES {
            return None;
        }
        let address = Address::new(address);
        let bytes = formatter.read_memory(address, size as usize).ok()?;
        Some(
            match runtime.system_mutex_state(address, &bytes)? {
                LockState::Unlocked => "unlocked",
                LockState::Locked => "locked",
            }
            .to_owned(),
        )
    }
}

/// Renders the value held by a `MutexGuard`, `RwLockReadGuard` or `RwLockWriteGuard`.
/// A `MutexGuard` includes a poison marker when its mutex is poisoned.
pub(super) struct GuardPrinter;

impl PrettyPrinter for GuardPrinter
{
    fn matches(&self, ty: &Type, _types: &TypeTable) -> bool
    {
        has_any_prefix(ty, MUTEX_GUARD_NAMES)
            || has_any_prefix(ty, READ_GUARD_NAMES)
            || has_any_prefix(ty, WRITE_GUARD_NAMES)
    }

    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
    {
        let Some(ty) = formatter.types().get(formatter.types().strip_aliases(value.ty)) else {
            return raw_layout(value, formatter);
        };
        let shown = if has_any_prefix(&ty, READ_GUARD_NAMES) {
            read_guard_data(value, formatter.types(), formatter.endian()).map(|data| ("RwLockReadGuard", false, data))
        } else {
            let is_mutex = has_any_prefix(&ty, MUTEX_GUARD_NAMES);
            let name = if is_mutex { "MutexGuard" } else { "RwLockWriteGuard" };
            guarded_lock(value, is_mutex, formatter).map(|(poisoned, data)| (name, poisoned, data))
        };
        let Some((name, poisoned, data)) = shown else {
            return raw_layout(value, formatter);
        };
        let data = data.format(formatter);
        if poisoned {
            format!("{name}(poisoned) {data}")
        } else {
            format!("{name} {data}")
        }
    }
}

enum PointerMetadata
{
    Thin,
    Length(u64),
    Vtable(DynMetadata),
}

struct GuardPointer
{
    address: Address,
    ty: TypeId,
    metadata: PointerMetadata,
}

impl GuardPointer
{
    fn from_pointer(value: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<Self>
    {
        let (address, ty, metadata) = if let Some((address, ty)) = thin_pointer(value, types, endian) {
            (address, ty, PointerMetadata::Thin)
        } else if let Some((address, len, ty)) = fat_pointer(value, types, endian) {
            (address, ty, PointerMetadata::Length(len))
        } else {
            let (address, ty, metadata) = DynMetadata::from_pointer(value, types, endian)?;
            (address, ty, PointerMetadata::Vtable(metadata))
        };
        Some(Self { address, ty, metadata })
    }

    fn format(self, formatter: &mut ValueFormatter<'_>) -> String
    {
        match self.metadata {
            PointerMetadata::Thin => formatter.format_at(self.ty, self.address),
            PointerMetadata::Length(len) => vec::format_elements(self.address, len, self.ty, formatter),
            PointerMetadata::Vtable(metadata) => metadata.format_at(self.address, self.ty, formatter),
        }
    }
}

/// The value a read guard's `data: NonNull<T>` points at.
fn read_guard_data(guard: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<GuardPointer>
{
    let pointer = field(field(guard, "data", types)?, "pointer", types)?;
    let data = GuardPointer::from_pointer(pointer, types, endian)?;
    let is_supported = match data.metadata {
        PointerMetadata::Thin => true,
        PointerMetadata::Length(_) => type_named(pointer, types, "*const [") || type_named(pointer, types, "*mut ["),
        PointerMetadata::Vtable(_) => types
            .get(types.strip_aliases(data.ty))
            .is_some_and(|ty| has_prefix(&ty, "dyn ")),
    };
    is_supported.then_some(data)
}

/// Follows a guard's `lock` reference to its data and checks for mutex poisoning.
fn guarded_lock(guard: ValueView<'_>, is_mutex: bool, formatter: &mut ValueFormatter<'_>) -> Option<(bool, GuardPointer)>
{
    let types = formatter.types();
    let endian = formatter.endian();
    let mut pointer = GuardPointer::from_pointer(field(guard, "lock", types)?, types, endian)?;
    let lock_ty = types.get(types.strip_aliases(pointer.ty))?;
    let lock_names = if is_mutex { MUTEX_NAMES } else { RW_LOCK_NAMES };
    if !has_any_prefix(&lock_ty, lock_names) || pointer.address == Address::ZERO {
        return None;
    }
    let TypeKind::Struct(layout) = &lock_ty.kind else {
        return None;
    };
    let data = layout.member("data")?;
    if data.bit_size.is_some() {
        return None;
    }
    let cell = types.get(types.strip_aliases(data.ty?))?;
    pointer.ty = unsafe_cell_member(&cell)?.ty?;
    let mut offset = data.offset?;
    let payload = types.get(types.strip_aliases(pointer.ty))?;
    match &pointer.metadata {
        PointerMetadata::Thin => {}
        PointerMetadata::Length(_) => {
            let TypeKind::Array(array) = &payload.kind else {
                return None;
            };
            if array.lengths != [None] {
                return None;
            }
            pointer.ty = array.element?;
        }
        PointerMetadata::Vtable(metadata) => {
            if !has_prefix(&payload, "dyn ") {
                return None;
            }
            // DWARF gives the tail offset at the trait object's minimum alignment.
            // Round it up to the alignment of the concrete type in this allocation.
            offset = offset.checked_next_multiple_of(metadata.alignment(formatter)?)?;
        }
    }
    let address = pointer.address;
    pointer.address = address.checked_add(offset)?;
    // Read the mutex flag separately so the data read stays subject to the formatter's limits.
    let poisoned = if is_mutex && let Some(flag) = layout.member("poison") {
        let flag_ty = flag.ty?;
        let flag_address = address.checked_add(flag.offset?)?;
        let size = usize::try_from(formatter.types().get(flag_ty)?.size?).ok()?;
        let bytes = formatter.read_memory(flag_address, size).ok()?;
        let view = ValueView {
            ty: flag_ty,
            bytes: &bytes,
            address: Some(flag_address),
        };
        poison_flag_set(Some(view), formatter.types(), endian)?
    } else {
        false
    };
    Some((poisoned, pointer))
}

fn rw_lock_state(inner: ValueView<'_>, types: &TypeTable, endian: RunTimeEndian) -> Option<String>
{
    let state = field(inner, "state", types)?;
    let raw = atomic_word(state, types, endian)?;
    if has_member(inner, "writer_notify", types) {
        return Some(match raw & FUTEX_RW_MASK {
            0 => "unlocked".to_owned(),
            FUTEX_RW_MASK => "write-locked".to_owned(),
            readers => format!("readers: {readers}"),
        });
    }
    let storage = atomic_storage(state, types)?;
    if !matches!(types.get(types.strip_aliases(storage.ty))?.kind, TypeKind::Pointer(_)) {
        return None;
    }
    Some(if raw & QUEUE_LOCKED == 0 {
        "unlocked".to_owned()
    } else if raw & QUEUE_QUEUED != 0 {
        // The reader count moves to the queue's tail node while waiters are queued.
        "locked".to_owned()
    } else {
        match raw / QUEUE_SINGLE {
            0 => "write-locked".to_owned(),
            readers => format!("readers: {readers}"),
        }
    })
}
