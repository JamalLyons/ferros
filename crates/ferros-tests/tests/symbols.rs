//! Source line lookups (`file:line` <-> address) against fixture dSYMs, for issue #35.
//!
//! These tests read fixture executables and their dSYM bundles only. They launch no
//! process and need no debugger entitlement, so they skip `harness::run` (the exception
//! AGENTS.md allows for tests that launch nothing); they still pass when the runner signs
//! without the entitlement (`FERROS_TEST_UNSIGNED=1`).
#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferros_core::symbols::{BinaryImage, DebugInfoOptions, FunctionAddress, ImageDescriptor, SourceLookupError};
use ferros_core::{Address, SymbolCache};
use ferros_tests::{Fixture, fixture};
use object::{Object, ObjectSegment, ObjectSymbol, SymbolKind};

/// Every fixture binary. `fixture!` needs a literal name, so the table is written out;
/// `fixture_table_covers_every_source` keeps it in step with `fixtures/`.
fn all_fixtures() -> Vec<Fixture>
{
    vec![
        fixture!("exit_code"),
        fixture!("hello"),
        fixture!("loop_counter"),
        fixture!("nested_calls"),
        fixture!("signal_handler"),
        fixture!("signal_tracing"),
        fixture!("spawn_thread"),
        fixture!("step_races"),
        fixture!("symbols"),
        fixture!("thread_signals"),
        fixture!("threads"),
        fixture!("workqueue"),
    ]
}

/// A scratch directory for the generated-DWARF cache (never used: every fixture has an
/// adjacent dSYM), removed on drop.
struct Scratch(PathBuf);

impl Scratch
{
    fn new() -> Self
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferros-symbols-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    fn options(&self) -> DebugInfoOptions
    {
        DebugInfoOptions {
            cache_directory: self.0.join("cache"),
            dsymutil: None,
            tool_timeout: Duration::from_secs(10),
        }
    }
}

impl Drop for Scratch
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Load `fixture` at its `__TEXT` vmaddr, so runtime and file addresses coincide.
fn load(fixture: &Fixture) -> Arc<BinaryImage>
{
    let bytes = fs::read(fixture.path()).expect("read executable");
    let file = object::File::parse(&*bytes).expect("parse executable");
    let text = file
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some("__TEXT"))
        .expect("__TEXT segment");
    let scratch = Scratch::new();
    let mut cache = SymbolCache::with_debug_info_options(scratch.options());
    cache
        .load_image(ImageDescriptor {
            path: fixture.path().to_path_buf(),
            load_address: text.address(),
        })
        .expect("fixture image with adjacent dSYM")
}

/// The source query used for a fixture: `fixtures/<name>.rs`.
fn source_query(fixture: &Fixture) -> String
{
    format!("fixtures/{}.rs", fixture.name())
}

fn source(fixture: &Fixture) -> String
{
    fs::read_to_string(fixture.source_path()).expect("read fixture source")
}

/// Marker names of a fixture source, in file order.
fn markers(source: &str) -> Vec<String>
{
    source
        .lines()
        .filter_map(|line| line.split_once("// BREAK:"))
        .filter_map(|(_, after)| after.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// Name of the closest `fn` declared at or above 1-based `line`.
fn enclosing_function(source: &str, line: u32) -> String
{
    let lines: Vec<&str> = source.lines().collect();
    let line = usize::try_from(line).expect("line");
    lines[..line]
        .iter()
        .rev()
        .find_map(|text| {
            let code = text.split("//").next().unwrap_or_default();
            let (_, after) = code.split_once("fn ")?;
            let before = &code[..code.len() - after.len() - 3];
            if !before.is_empty() && !before.ends_with(' ') {
                return None;
            }
            let name: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            (!name.is_empty()).then_some(name)
        })
        .expect("enclosing fn")
}

/// Last path segment of a demangled name, generic arguments removed:
/// `<crate::Counter>::bump` -> `bump`, `crate::generic_identity::<u64>` -> `generic_identity`.
fn last_segment(name: &str) -> String
{
    let mut depth = 0_usize;
    let mut plain = String::new();
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    plain
        .split("::")
        .filter(|part| !part.is_empty())
        .last()
        .unwrap_or_default()
        .to_owned()
}

/// Whether any frame (inlined ones included) of `address` belongs to function `function`.
fn in_function(image: &BinaryImage, address: Address, function: &str) -> bool
{
    image.symbolicate(address).is_some_and(|result| {
        result.frames.iter().any(|frame| {
            let raw = frame.symbol.raw();
            let name = rustc_demangle::try_demangle(raw).map_or_else(|_| raw.to_owned(), |name| format!("{name:#}"));
            last_segment(&name) == function
        })
    })
}

#[test]
fn fixture_table_covers_every_source()
{
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let sources: BTreeSet<String> = fs::read_dir(&dir)
        .expect("read fixtures directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| path.file_stem().expect("stem").to_string_lossy().into_owned())
        .collect();
    let table: BTreeSet<String> = all_fixtures().iter().map(|fixture| fixture.name().to_owned()).collect();
    assert_eq!(table, sources, "all_fixtures() must list every fixtures/*.rs");
}

#[test]
fn every_marker_resolves_inside_its_function_and_maps_back()
{
    for fixture in all_fixtures() {
        let image = load(&fixture);
        let text = source(&fixture);
        let query = source_query(&fixture);
        for marker in markers(&text) {
            let line = fixture.marker_line(&marker);
            let function = enclosing_function(&text, line);
            let at = format!("{}:{marker}", fixture.name());
            let found = image
                .line_addresses(&query, line)
                .unwrap_or_else(|error| panic!("{at}: {error}"));
            assert_eq!(found.line, line, "{at} has code on its own line");
            assert!(found.file.ends_with(&query), "{at}: {}", found.file);
            assert!(!found.addresses.is_empty(), "{at} has no address");
            for &address in &found.addresses {
                let at = format!("{at} address {address}");
                assert!(in_function(&image, address, &function), "{at} is not inside fn {function}");
                let back = image
                    .line_at_address(address)
                    .expect("line tables")
                    .unwrap_or_else(|| panic!("{at} has no line"));
                assert_eq!(back.file, found.file, "{at}");
                assert_eq!(back.line, line, "{at}");
            }
        }
    }
}

/// Generic markers of the `symbols` fixture and the function each marker is in. Each
/// function is instantiated for `u64` and `u32`.
const GENERIC_MARKERS: &[(&str, &str)] = &[
    ("in_generic", "generic_identity"),
    // Written on one line, so both copies have rows only for this line, back to back in
    // one line-table sequence.
    ("one_line_generic", "one_line_identity"),
];

/// Demangled names (without hash) of the innermost frame of `name` at `address`.
fn frame_name(image: &BinaryImage, address: Address, function: &str) -> String
{
    let frames = image.symbolicate(address).expect("symbolicated").frames;
    frames
        .iter()
        .map(|frame| {
            let raw = frame.symbol.raw();
            rustc_demangle::try_demangle(raw).map_or_else(|_| raw.to_owned(), |name| format!("{name:#}"))
        })
        .find(|name| last_segment(name) == function)
        .unwrap_or_else(|| panic!("{address} is not in fn {function}"))
}

#[test]
fn generic_and_inlined_markers_have_one_address_per_copy()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    for &(marker, function) in GENERIC_MARKERS {
        let found = image
            .line_addresses("fixtures/symbols.rs", fixture.marker_line(marker))
            .expect("generic marker");
        assert_eq!(found.addresses.len(), 2, "{marker}: one per instantiation: {found:?}");
        let names: BTreeSet<String> = found
            .addresses
            .iter()
            .map(|&address| frame_name(&image, address, function))
            .collect();
        assert_eq!(names.len(), 2, "{marker}: each address in its own instantiation: {names:?}");
        for name in &names {
            assert!(name.ends_with("::<u64>") || name.ends_with("::<u32>"), "{marker}: {name}");
        }
    }
    let inlined = image
        .line_addresses("fixtures/symbols.rs", fixture.marker_line("in_inlined"))
        .expect("inlined marker");
    assert!(inlined.addresses.len() >= 2, "one per call site: {inlined:?}");
}

#[test]
fn line_ranges_stay_inside_their_own_function()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    for &(marker, function) in GENERIC_MARKERS {
        for instance in image.function_addresses(function).expect("generic function") {
            let at = image
                .line_at_address(instance.after_prologue)
                .expect("line tables")
                .expect("mapped");
            assert_eq!(at.line, fixture.marker_line(marker), "{marker}: {instance:?}");
            for range in &at.ranges {
                let inside = instance.entry <= range.start && range.end <= instance.end;
                assert!(inside, "{marker}: range {range:?} leaves {instance:?}");
            }
        }
    }
}

#[test]
fn line_without_code_resolves_to_next_line_with_code()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    let after_gap = fixture.marker_line("after_gap");
    let gap = after_gap - 1;
    let text = source(&fixture);
    let gap_text = text
        .lines()
        .nth(usize::try_from(gap - 1).expect("line"))
        .expect("gap line")
        .trim();
    assert!(
        gap_text.is_empty() || gap_text.starts_with("//"),
        "line {gap} must have no code: {gap_text:?}"
    );
    assert_eq!(fixture.marker_line("before_gap"), gap - 1);
    let found = image.line_addresses("fixtures/symbols.rs", gap).expect("gap line");
    assert_eq!(found.line, after_gap);
    let direct = image.line_addresses("fixtures/symbols.rs", after_gap).expect("after_gap");
    assert_eq!(found.addresses, direct.addresses);
}

#[test]
fn path_queries_match_on_component_boundaries()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    let line = fixture.marker_line("in_method");
    let expected = image.line_addresses("fixtures/symbols.rs", line).expect("suffix");

    let absolute = fixture.source_path();
    let exact = image
        .line_addresses(absolute.to_str().expect("utf-8 path"), line)
        .expect("absolute path");
    assert_eq!(exact, expected);
    assert_eq!(exact.file, absolute.to_str().expect("utf-8 path"));

    let bare = image.line_addresses("symbols.rs", line).expect("file name only");
    assert_eq!(bare, expected);
    let partial = image.line_addresses("ymbols.rs", line);
    assert!(
        matches!(partial, Err(SourceLookupError::UnknownFile { .. })),
        "a partial component must not match: {partial:?}"
    );
}

#[test]
fn lookup_errors_are_typed()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);

    let unknown = image.line_addresses("fixtures/no_such_file.rs", 1);
    assert!(matches!(unknown, Err(SourceLookupError::UnknownFile { .. })), "{unknown:?}");

    let past_end = image.line_addresses("fixtures/symbols.rs", 100_000);
    let Err(SourceLookupError::NoCodeAtOrAfterLine { file, line, last_line }) = past_end else {
        panic!("expected NoCodeAtOrAfterLine: {past_end:?}");
    };
    assert!(file.ends_with("fixtures/symbols.rs"), "{file}");
    assert_eq!(line, 100_000);
    assert!(last_line >= fixture.marker_line("after_gap"), "{last_line}");

    let zero = image.line_addresses("fixtures/symbols.rs", 0);
    assert!(matches!(zero, Err(SourceLookupError::InvalidLine { line: 0 })), "{zero:?}");

    // Inlined standard-library code puts several distinct `mod.rs` files in the tables.
    let ambiguous = image.line_addresses("mod.rs", 1);
    let Err(SourceLookupError::AmbiguousFile { candidates, .. }) = ambiguous else {
        panic!("expected AmbiguousFile: {ambiguous:?}");
    };
    assert!(candidates.len() >= 2, "{candidates:?}");
    assert!(candidates.iter().all(|path| path.ends_with("/mod.rs")), "{candidates:?}");
    assert!(
        candidates.windows(2).all(|pair| pair[0] < pair[1]),
        "sorted and distinct: {candidates:?}"
    );
}

/// `symbols` fixture markers whose line ranges are checked.
const RANGE_MARKERS: &[&str] = &[
    "in_generic",
    "one_line_generic",
    "in_method",
    "in_module",
    "in_inlined",
    "after_gap",
];

#[test]
fn address_line_ranges_cover_the_marker_and_map_back()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    for &marker in RANGE_MARKERS {
        let line = fixture.marker_line(marker);
        let found = image.line_addresses("fixtures/symbols.rs", line).expect("marker");
        for &address in &found.addresses {
            let at = image.line_at_address(address).expect("line tables").expect("mapped");
            assert_eq!((at.file.as_str(), at.line), (found.file.as_str(), line), "{marker}");
            // rustc records a column on every row except a function's entry row; a
            // one-line function's first statement row is its entry.
            if marker != "one_line_generic" {
                assert!(at.column.is_some(), "{marker}: rustc records columns");
            }
            assert!(!at.ranges.is_empty(), "{marker}");
            assert!(at.ranges.iter().any(|range| range.contains(&address)), "{marker}: {at:?}");
            assert!(
                at.ranges.windows(2).all(|pair| pair[0].end < pair[1].start),
                "{marker}: sorted and merged: {at:?}"
            );
            for range in &at.ranges {
                assert!(range.start < range.end, "{marker}: {range:?}");
                for probe in [range.start, Address::new(range.end.value() - 1)] {
                    let back = image.line_at_address(probe).expect("line tables").expect("mapped");
                    assert_eq!(
                        (back.file.as_str(), back.line),
                        (found.file.as_str(), line),
                        "{marker} {probe}"
                    );
                }
            }
        }
    }
    assert_eq!(image.line_at_address(Address::new(0x10)).expect("line tables"), None);
}

#[test]
fn lookups_after_first_load_take_under_a_second()
{
    let limit = Duration::from_secs(1);
    for fixture in all_fixtures() {
        let image = load(&fixture);
        let query = source_query(&fixture);
        let text = source(&fixture);
        // The first lookup builds the index; time every later one.
        let _ = image.line_addresses(&query, 1);
        for marker in markers(&text) {
            let line = fixture.marker_line(&marker);
            let started = Instant::now();
            let found = image.line_addresses(&query, line).expect("marker");
            assert!(
                started.elapsed() < limit,
                "{}:{marker} took {:?}",
                fixture.name(),
                started.elapsed()
            );
            for address in found.addresses {
                let started = Instant::now();
                image.line_at_address(address).expect("line tables").expect("mapped");
                assert!(
                    started.elapsed() < limit,
                    "{} {address} took {:?}",
                    fixture.name(),
                    started.elapsed()
                );
            }
        }
    }
}

/// `nested_calls` functions and the marker on their first statement.
const NESTED_FUNCTIONS: &[(&str, &str)] = &[("level_a", "in_a"), ("level_b", "in_b"), ("level_c", "in_c")];

/// Function queries and the demangled symbol-table name of the function they resolve to.
const SYMBOL_TABLE_FUNCTIONS: &[(&str, &str)] = &[
    ("shapes::area", "ferros_fixture_symbols::shapes::area"),
    ("Counter::bump", "<ferros_fixture_symbols::Counter>::bump"),
    ("with_gap", "ferros_fixture_symbols::with_gap"),
];

/// Empty or malformed function names.
const INVALID_FUNCTION_NAMES: &[&str] = &["", "   ", "shapes::", "::area", "a::::b", "area<"];

/// Function queries timed after the index is built (found, ambiguous and unknown).
const TIMED_FUNCTION_QUERIES: &[&str] = &[
    "generic_identity",
    "Counter::bump",
    "shapes::area",
    "ferros_fixture_symbols::shapes::area",
    "with_gap",
    "fmt",
    "no_such_function",
];

/// The single function `name` resolves to in `image`.
fn one_function(image: &BinaryImage, name: &str) -> FunctionAddress
{
    let found = image
        .function_addresses(name)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    let [function] = found.as_slice() else {
        panic!("{name}: expected one function, found {found:#?}");
    };
    function.clone()
}

/// Check the invariants of one resolved function: its entry is inside the function
/// `function` (by symbolication), and its post-prologue address lies strictly inside it
/// and is on the line of `marker`.
///
/// The line is checked two ways. `function`'s own frame at the post-prologue address
/// (the outermost frame of that name) must be on the marker line. When the line-table
/// row there belongs to the fixture source, it must be the marker line too; when the
/// first statement starts with code inlined from the standard library (`area`'s
/// `wrapping_mul`), the row names the library file and only the frame check applies.
fn check_function(fixture: &Fixture, image: &BinaryImage, found: &FunctionAddress, function: &str, marker: &str)
{
    let at = format!("{}: {found:?}", fixture.name());
    let marker_line = fixture.marker_line(marker);
    let query = source_query(fixture);
    assert!(
        in_function(image, found.entry, function),
        "{at}: entry is not in fn {function}"
    );
    assert!(
        found.entry < found.after_prologue && found.after_prologue < found.end,
        "{at}: post-prologue address outside the function body"
    );
    let line = image
        .line_at_address(found.after_prologue)
        .expect("line tables")
        .unwrap_or_else(|| panic!("{at}: post-prologue address has no line"));
    if line.file.ends_with(&query) {
        assert_eq!(line.line, marker_line, "{at}: post-prologue line");
    }
    let frames = image.symbolicate(found.after_prologue).expect("symbolicated").frames;
    let frame = frames
        .iter()
        .rev()
        .find(|frame| {
            let raw = frame.symbol.raw();
            let name = rustc_demangle::try_demangle(raw).map_or_else(|_| raw.to_owned(), |name| format!("{name:#}"));
            last_segment(&name) == function
        })
        .unwrap_or_else(|| panic!("{at}: no frame of fn {function}: {frames:#?}"));
    let location = frame.location.as_ref().expect("frame location");
    assert!(location.file.ends_with(&query), "{at}: {location:?}");
    assert_eq!(location.line, Some(marker_line), "{at}: frame line");
}

/// File virtual address of the defined Mach-O text symbol whose demangled name (without
/// hash) is `wanted`.
fn text_symbol(fixture: &Fixture, wanted: &str) -> u64
{
    let bytes = fs::read(fixture.path()).expect("read executable");
    let file = object::File::parse(&*bytes).expect("parse executable");
    let mut addresses: Vec<u64> = file
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.is_definition())
        .filter(|symbol| {
            symbol
                .name()
                .ok()
                .and_then(|raw| rustc_demangle::try_demangle(raw).ok())
                .is_some_and(|name| format!("{name:#}") == wanted)
        })
        .map(|symbol| symbol.address())
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    let [address] = addresses.as_slice() else {
        panic!("expected one text symbol {wanted}, found {addresses:#x?}");
    };
    *address
}

#[test]
fn generic_function_resolves_to_one_entry_per_instantiation()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    let found = image.function_addresses("generic_identity").expect("generic_identity");
    assert_eq!(found.len(), 2, "one per instantiation: {found:#?}");
    assert!(found[0].entry < found[1].entry, "distinct and sorted: {found:#?}");
    let names: BTreeSet<&str> = found.iter().map(|function| function.name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "ferros_fixture_symbols::generic_identity::<u32>",
            "ferros_fixture_symbols::generic_identity::<u64>",
        ])
    );
    for function in &found {
        assert_eq!(function.path, "ferros_fixture_symbols::generic_identity");
        check_function(&fixture, &image, function, "generic_identity", "in_generic");
    }
}

#[test]
fn methods_module_functions_and_trailing_paths_resolve()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);

    let bump = one_function(&image, "Counter::bump");
    assert_eq!(bump.path, "ferros_fixture_symbols::Counter::bump");
    assert_eq!(bump.name, "<ferros_fixture_symbols::Counter>::bump");
    check_function(&fixture, &image, &bump, "bump", "in_method");
    assert_eq!(one_function(&image, "<ferros_fixture_symbols::Counter>::bump"), bump);

    let area = one_function(&image, "ferros_fixture_symbols::shapes::area");
    assert_eq!(area.path, "ferros_fixture_symbols::shapes::area");
    for query in ["shapes::area", "area"] {
        assert_eq!(one_function(&image, query), area, "{query}");
    }
    check_function(&fixture, &image, &area, "area", "in_module");

    let gap = one_function(&image, "with_gap");
    check_function(&fixture, &image, &gap, "with_gap", "before_gap");

    // Only ever inlined: no out-of-line copy, so no entry address.
    let inlined = image.function_addresses("always_inlined");
    assert!(
        matches!(inlined, Err(SourceLookupError::UnknownFunction { .. })),
        "{inlined:?}"
    );

    let fixture = fixture!("nested_calls");
    let image = load(&fixture);
    for &(function, marker) in NESTED_FUNCTIONS {
        let found = one_function(&image, function);
        check_function(&fixture, &image, &found, function, marker);
    }
}

#[test]
fn entry_matches_the_mach_o_symbol_table()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    // `load` maps the image at its vmaddr, so runtime and file addresses coincide.
    for &(query, symbol) in SYMBOL_TABLE_FUNCTIONS {
        let found = one_function(&image, query);
        assert_eq!(found.entry.value(), text_symbol(&fixture, symbol), "{query}");
    }
}

#[test]
fn main_resolves_to_the_crate_main()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    let crate_main = one_function(&image, "ferros_fixture_symbols::main");
    assert!(in_function(&image, crate_main.entry, "main"), "{crate_main:?}");
    // The C `main` that rustc generates has no DWARF subprogram, and no standard-library
    // function with code has a path ending in `main`, so `main` matches only the crate's
    // `main`. Were a function with the whole path `main` present, it would win (the
    // exact-path rule, covered by the ferros-core unit tests), not be ambiguous.
    assert_eq!(image.function_addresses("main"), Ok(vec![crate_main]));
}

#[test]
fn function_lookup_errors_are_typed()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);

    let unknown = image.function_addresses("no_such_function");
    assert!(
        matches!(&unknown, Err(SourceLookupError::UnknownFunction { query }) if query == "no_such_function"),
        "{unknown:?}"
    );
    // A partial segment does not match.
    let partial = image.function_addresses("hapes::area");
    assert!(
        matches!(partial, Err(SourceLookupError::UnknownFunction { .. })),
        "{partial:?}"
    );

    for &query in INVALID_FUNCTION_NAMES {
        let invalid = image.function_addresses(query);
        assert!(
            matches!(invalid, Err(SourceLookupError::InvalidFunctionName { .. })),
            "{query:?}: {invalid:?}"
        );
    }

    // The standard library code in the fixture's DWARF has many `fmt` implementations.
    let ambiguous = image.function_addresses("fmt");
    let Err(SourceLookupError::AmbiguousFunction { query, candidates }) = ambiguous else {
        panic!("expected AmbiguousFunction: {ambiguous:?}");
    };
    assert_eq!(query, "fmt");
    assert!(candidates.len() >= 2, "{candidates:?}");
    assert!(candidates.iter().all(|path| path.ends_with("::fmt")), "{candidates:?}");
    assert!(
        candidates.windows(2).all(|pair| pair[0] < pair[1]),
        "sorted and distinct: {candidates:?}"
    );
}

#[test]
fn function_lookups_after_first_load_take_under_a_second()
{
    let limit = Duration::from_secs(1);
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    // The first lookup builds the index; time every later one.
    let _ = image.function_addresses("main");
    for &query in TIMED_FUNCTION_QUERIES {
        let started = Instant::now();
        let _ = image.function_addresses(query);
        assert!(started.elapsed() < limit, "{query} took {:?}", started.elapsed());
    }
}
