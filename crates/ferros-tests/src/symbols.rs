//! Fixture symbol lookup from the Mach-O symbol table (macOS only).
//!
//! The engine has no `function` breakpoints yet, and resolving a source marker needs the
//! fixture's DWARF. A function's entry address only needs the executable's symbol table,
//! which every fixture build has.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use ferros_core::Address;
use object::{Object, ObjectSegment, ObjectSymbol, SymbolKind};

use crate::Fixture;
use crate::harness::Debuggee;

/// Runtime address of the first instruction of the fixture function `name`.
///
/// `name` is the function's path inside the fixture crate, without the crate: `"tick"`
/// for `fn tick` of `loop_counter`, `"worker_body"` for `threads`. The fixture's crate is
/// named after its binary (`ferros-fixture-loop_counter` is the crate
/// `ferros_fixture_loop_counter`), so the symbol looked up is the defined text symbol
/// whose demangled name (without hash) is `ferros_fixture_loop_counter::tick`. Both the
/// legacy and the v0 mangling are understood.
///
/// The address comes from the Mach-O symbol table of [`Fixture::path`] (no DWARF) plus
/// the slide of the running image:
///
/// ```text
/// runtime = load address of the main executable (first entry of `d.images()`)
///         + (symbol vmaddr - vmaddr of the `__TEXT` segment)
/// ```
///
/// The offset is cached per fixture and function, so later launches only pay for the
/// `images` call. Mark the function `#[inline(never)]` in the fixture, or it may have no
/// symbol of its own.
///
/// # Panics
///
/// Panics if the session is not stopped (`images` fails) or reports no image, if the
/// executable cannot be read or parsed, has no `__TEXT` segment, or if no text symbol, or
/// more than one at different addresses, has that name.
pub fn function_address(d: &mut Debuggee, fixture: &Fixture, name: &str) -> Address
{
    static OFFSETS: OnceLock<Mutex<HashMap<(PathBuf, String), u64>>> = OnceLock::new();

    let images = d.images().expect("images");
    let main = images.first().expect("the main executable image");
    let load_address = main.load_address.value();

    let key = (fixture.path().to_path_buf(), name.to_owned());
    let offsets = OFFSETS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(offset) = offsets.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return Address::from(load_address + offset);
    }

    let offset = text_offset(fixture, name);
    offsets.lock().unwrap_or_else(PoisonError::into_inner).insert(key, offset);
    Address::from(load_address + offset)
}

/// Offset of the fixture function `name` from the start of the `__TEXT` segment.
fn text_offset(fixture: &Fixture, name: &str) -> u64
{
    let path = fixture.path();
    let data = std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let file = object::File::parse(&*data).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));

    let text = file
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some("__TEXT"))
        .unwrap_or_else(|| panic!("{} has no __TEXT segment", path.display()));

    let binary = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("fixture path {} has no file name", path.display()));
    let wanted = format!("{}::{name}", binary.replace('-', "_"));

    let mut addresses: Vec<u64> = file
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.is_definition())
        .filter(|symbol| {
            symbol
                .name()
                .ok()
                .and_then(|raw| rustc_demangle::try_demangle(raw).ok())
                .is_some_and(|demangled| format!("{demangled:#}") == wanted)
        })
        .map(|symbol| symbol.address())
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    let [address] = addresses.as_slice() else {
        // Zero or several different addresses (`function_address` documents the panic).
        panic!(
            "expected one text symbol named {wanted} in {}, found {addresses:#x?}",
            path.display()
        );
    };
    address - text.address()
}
