//! Fixture symbol lookup from the Mach-O symbol table (macOS only).
//!
//! The engine has no `function` breakpoints yet, and resolving a source marker needs the
//! fixture's DWARF. A function's entry address only needs the executable's symbol table,
//! which every fixture build has.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use ferros_core::{Address, Session};
use object::{Object, ObjectSymbol, SymbolKind};

use crate::Fixture;

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
/// the session's per-image mapping. The executable symbol table supplies the file
/// virtual address, and the loader supplies the runtime header address.
///
/// `session` is any stopped session on that fixture: a harness `Debuggee` (through
/// deref, `function_address(&mut d, ..)`) or a `Session` attached to a spawned fixture.
/// The file address is cached per fixture and function, so later launches only pay for the
/// `images` call. Mark the function `#[inline(never)]` in the fixture, or it may have no
/// symbol of its own.
///
/// # Panics
///
/// Panics if the session is not stopped (`images` fails) or reports no image, if the
/// executable cannot be read or parsed or mapped, or if no text symbol, or
/// more than one at different addresses, has that name.
pub fn function_address(session: &mut Session, fixture: &Fixture, name: &str) -> Address
{
    static ADDRESSES: OnceLock<Mutex<HashMap<(PathBuf, String), u64>>> = OnceLock::new();

    let images = session.images().expect("images");
    let main = images
        .iter()
        .find(|image| image.path == fixture.path())
        .expect("the main executable image");
    let key = (fixture.path().to_path_buf(), name.to_owned());
    let addresses = ADDRESSES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut addresses = addresses.lock().unwrap_or_else(PoisonError::into_inner);
    let file_address = *addresses.entry(key).or_insert_with(|| symbol_address(fixture, name));
    session
        .runtime_address(main, file_address)
        .expect("image mapping")
        .expect("mapped symbol")
}

/// File virtual address of fixture function `name` from its symbol table.
fn symbol_address(fixture: &Fixture, name: &str) -> u64
{
    let path = fixture.path();
    let data = std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let file = object::File::parse(&*data).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));

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
    *address
}
