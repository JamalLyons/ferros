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

use ferros_core::symbols::{BinaryImage, DebugInfoOptions, ImageDescriptor, SourceLookupError};
use ferros_core::{Address, SymbolCache};
use ferros_tests::{Fixture, fixture};
use object::{Object, ObjectSegment};

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

#[test]
fn generic_and_inlined_markers_have_one_address_per_copy()
{
    let fixture = fixture!("symbols");
    let image = load(&fixture);
    let generic = image
        .line_addresses("fixtures/symbols.rs", fixture.marker_line("in_generic"))
        .expect("generic marker");
    assert_eq!(generic.addresses.len(), 2, "one per instantiation: {generic:?}");
    assert_ne!(generic.addresses[0], generic.addresses[1]);
    let inlined = image
        .line_addresses("fixtures/symbols.rs", fixture.marker_line("in_inlined"))
        .expect("inlined marker");
    assert!(inlined.addresses.len() >= 2, "one per call site: {inlined:?}");
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
const RANGE_MARKERS: &[&str] = &["in_generic", "in_method", "in_module", "in_inlined", "after_gap"];

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
            assert!(at.column.is_some(), "{marker}: rustc records columns");
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
