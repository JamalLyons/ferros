//! Loader-reported image identity: the Mach-O UUID next to path and load address
//! (issue #121), the mapped-header UUID cache (issue #182), and path aliasing (#347).
#![cfg(target_os = "macos")]

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferros_core::{Address, BreakpointRequest, Session, StopEvent};
use ferros_macos::images::{
    invalidate_mapped_uuid_cache, mapped_uuid_header_reads, reread_mapped_uuids, reset_mapped_uuid_header_reads,
    without_mapped_uuid_reads,
};
use ferros_tests::symbols::marker_address;
use ferros_tests::{fixture, harness};
use object::Object;

/// How long a test waits for one stop after resuming.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Resume until a breakpoint at `address` stops the process.
fn run_to(session: &mut harness::Debuggee, address: Address)
{
    session
        .add_breakpoint(BreakpointRequest::Software { address })
        .unwrap_or_else(|err| panic!("add_breakpoint at {address}: {err}"));
    session.resume().expect("resume");
    match session.wait(Some(STOP_TIMEOUT)).expect("wait") {
        Some(StopEvent::Breakpoint { pc, thread }) if pc == address => {
            session.select_thread(thread).expect("select the stopped thread");
        }
        other => panic!(
            "expected a breakpoint stop at {address}, got {other:?}; fixture stderr: {}",
            session.stderr()
        ),
    }
}

/// `LC_UUID` of the Mach-O file at `path`.
fn file_uuid(path: &Path) -> [u8; 16]
{
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let file = object::File::parse(&*bytes).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    file.mach_uuid()
        .unwrap_or_else(|error| panic!("uuid of {}: {error}", path.display()))
        .unwrap_or_else(|| panic!("{} has no LC_UUID", path.display()))
}

/// dyld publishes one row per mapped header even after a library is opened through aliases.
#[cfg(target_arch = "aarch64")]
#[test]
fn dyld_reports_one_image_for_library_path_aliases()
{
    use std::collections::HashMap;
    use std::os::unix::fs::symlink;

    use ferros_tests::scratch;
    use ferros_tests::symbols::raw_symbol_address;

    harness::run(|t| {
        let directory = scratch::create_dir("ferros-dyld-aliases");
        t.remove_on_cleanup(&directory);
        let directory = directory.canonicalize().expect("canonical scratch directory");
        let library = directory.join("libferros_alias_fixture.dylib");
        let built_library = Path::new(env!("OUT_DIR")).join("libferros_alias_fixture.dylib");
        fs::copy(built_library, &library).expect("copy alias fixture library");
        symlink(&library, directory.join("symlink.dylib")).expect("create library symlink");
        fs::hard_link(&library, directory.join("hardlink.dylib")).expect("create library hard link");

        let fixture = fixture!("dyld_aliases");
        let args = vec![
            directory.to_str().expect("UTF-8 scratch path"),
            library.to_str().expect("UTF-8 library path"),
            "./libferros_alias_fixture.dylib",
            "./symlink.dylib",
            "./hardlink.dylib",
        ]
        .into_boxed_slice();
        let mut d = t.launch_with_args(&fixture, &args);
        let loaded = raw_symbol_address(&mut d, &fixture, "_aliases_loaded");
        run_to(&mut d, loaded);

        let images = d.images().expect("images after opening library aliases");
        let mut paths_by_address = HashMap::new();
        for image in &images {
            let previous_path = paths_by_address.insert(image.load_address, &image.path);
            assert!(
                previous_path.is_none(),
                "dyld reported two rows at {}: {previous_path:?} and {:?}",
                image.load_address,
                image.path,
            );
        }
        let build_id = file_uuid(&library).into();
        let library_images: Vec<_> = images
            .iter()
            .filter(|image| image.build_id.known() == Some(build_id))
            .collect();
        assert_eq!(library_images.len(), 1, "expected one fixture library row: {images:#?}");
        eprintln!(
            "dyld alias probe: absolute, relative, symlink, hard link; {} rows at {} distinct addresses; {} fixture \
             library row",
            images.len(),
            paths_by_address.len(),
            library_images.len(),
        );
    });
}

/// The main executable's reported UUID is the `LC_UUID` of the fixture file. dyld's
/// `uuidArray` covers images that are not in the shared cache; the fixture is one. Images
/// the array does not cover (shared-cache libraries, and dyld when it lives in the cache)
/// stay `None`, so this asserts only the UUID the backend fills.
#[test]
fn main_executable_uuid_matches_its_file()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let in_c = marker_address(&mut d, &fixture, "in_c");
        run_to(&mut d, in_c);

        let images = d.images().expect("images");
        let main = images
            .iter()
            .find(|image| image.path == fixture.path())
            .expect("the main executable image");
        assert_eq!(
            main.build_id,
            ferros_core::BuildIdentity::Known(file_uuid(fixture.path()).into()),
            "{main:?}"
        );
    });
}

/// A second `images()` call with the same loader list does not read mapped headers for
/// images it already knows. The first call, after the cache is dropped, does: shared-cache
/// images are absent from dyld's UUID array, so a cache that is never filled would report
/// zero reads both times (#182).
#[test]
fn second_images_call_does_not_reread_known_headers()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let in_c = marker_address(&mut d, &fixture, "in_c");
        run_to(&mut d, in_c);

        invalidate_mapped_uuid_cache();
        reset_mapped_uuid_header_reads();
        let first = d.images().expect("images");
        let cold_reads = reset_mapped_uuid_header_reads();
        assert!(
            cold_reads > 0,
            "the first images() call after a cache drop read no mapped headers ({cold_reads}); dyld's UUID array covered \
             all {} images, so this would not show the cache working: {first:#?}",
            first.len()
        );
        assert!(first.iter().any(|image| image.build_id.known().is_some()), "{first:#?}");

        let second = d.images().expect("images again");
        let warm_reads = mapped_uuid_header_reads();
        assert_eq!(
            warm_reads, 0,
            "an unchanged image list re-read mapped headers {warm_reads} times"
        );
        assert_eq!(second, first, "the cached UUIDs differ from the headers just read");
    });
}

/// Median of an odd-length sample list. `samples` is not empty.
fn median(samples: &mut [Duration]) -> Duration
{
    samples.sort();
    samples[samples.len() / 2]
}

fn time_images(session: &mut Session, samples: usize) -> Duration
{
    let mut elapsed = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        session.images().expect("images");
        elapsed.push(started.elapsed());
    }
    median(&mut elapsed)
}

fn time_backtrace(session: &mut Session, samples: usize) -> (Duration, usize)
{
    let mut elapsed = Vec::with_capacity(samples);
    let mut frames = 0;
    for _ in 0..samples {
        let started = Instant::now();
        frames = session.stack_trace(256).expect("stack trace").len();
        elapsed.push(started.elapsed());
    }
    (median(&mut elapsed), frames)
}

/// Where the macOS integration job prints this measurement from.
fn measurement_path() -> PathBuf
{
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target")
        .join("ferros-uuid-measure.txt")
}

/// Time `images()` and a deep backtrace before #170 (no mapped UUID reads), after #170
/// (a mapped read every call), and with the load-address cache. The numbers are the
/// measurement #182 asks for; this host's CI log records the file the test writes.
/// Nothing here asserts a duration: the read counts are the functional check.
#[test]
fn measure_mapped_uuid_and_backtrace_cost()
{
    const SAMPLES: usize = 5;
    harness::run(|t| {
        let fixture = fixture!("recursion");
        let mut d = t.launch_with_args(&fixture, &["60"]);
        let bottom = marker_address(&mut d, &fixture, "bottom");
        run_to(&mut d, bottom);
        // Load symbols once so later traces time the image list, not DWARF parsing.
        let warmup_frames = d.stack_trace(256).expect("warmup trace").len();
        assert!(warmup_frames > 50, "{warmup_frames} frames");

        let image_count = d.images().expect("images").len();

        let before_images_us;
        let before_backtrace_us;
        let before_reads;
        {
            let _guard = without_mapped_uuid_reads();
            reset_mapped_uuid_header_reads();
            d.images().expect("images before");
            before_reads = reset_mapped_uuid_header_reads();
            before_images_us = time_images(&mut d, SAMPLES).as_micros();
            before_backtrace_us = time_backtrace(&mut d, 3).0.as_micros();
        }

        let after_images_us;
        let after_backtrace_us;
        let after_reads;
        {
            let _guard = reread_mapped_uuids();
            reset_mapped_uuid_header_reads();
            d.images().expect("images after");
            after_reads = reset_mapped_uuid_header_reads();
            after_images_us = time_images(&mut d, SAMPLES).as_micros();
            after_backtrace_us = time_backtrace(&mut d, 3).0.as_micros();
        }

        invalidate_mapped_uuid_cache();
        reset_mapped_uuid_header_reads();
        let cold_started = Instant::now();
        let cached = d.images().expect("images cold cache");
        let cached_cold_images_us = cold_started.elapsed().as_micros();
        let cached_cold_reads = reset_mapped_uuid_header_reads();
        let cached_warm_images_us = time_images(&mut d, SAMPLES).as_micros();
        let cached_warm_reads = mapped_uuid_header_reads();
        let (cached_backtrace, frames) = time_backtrace(&mut d, 3);
        let cached_backtrace_us = cached_backtrace.as_micros();
        let cached_backtrace_reads = mapped_uuid_header_reads().saturating_sub(cached_warm_reads);

        // One field per `write`: a single `format!` of every sample is a stack array
        // larger than clippy's `large_stack_arrays` limit.
        let mut line = String::new();
        let mut put = |key: &str, value: u128| {
            if !line.is_empty() {
                line.push(' ');
            }
            write!(line, "{key}={value}").expect("format a measurement field");
        };
        put("images", u128::from(u64::try_from(image_count).expect("image count")));
        put("frames", u128::from(u64::try_from(frames).expect("frame count")));
        put("before_images_us", before_images_us);
        put("before_images_header_reads", u128::from(before_reads));
        put("before_backtrace_us", before_backtrace_us);
        put("after_images_us", after_images_us);
        put("after_images_header_reads", u128::from(after_reads));
        put("after_backtrace_us", after_backtrace_us);
        put("cached_cold_images_us", cached_cold_images_us);
        put("cached_cold_header_reads", u128::from(cached_cold_reads));
        put("cached_warm_images_us", cached_warm_images_us);
        put("cached_warm_header_reads", u128::from(cached_warm_reads));
        put("cached_backtrace_us", cached_backtrace_us);
        put("cached_backtrace_header_reads", u128::from(cached_backtrace_reads));
        let path = measurement_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
        }
        eprintln!("FERROS_UUID_MEASURE {line}");
        line.push('\n');
        fs::write(&path, &line).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));

        assert_eq!(before_reads, 0, "skipping mapped reads still read headers: {line}");
        assert!(
            after_reads > 0,
            "repeating mapped reads performed none; the before/after comparison is vacuous: {line}"
        );
        assert_eq!(
            cached_cold_reads, after_reads,
            "a cold cache should read the same headers as an uncached call: {line}"
        );
        assert_eq!(cached_warm_reads, 0, "a warm cache re-read mapped headers: {line}");
        assert_eq!(
            cached_backtrace_reads, 0,
            "a backtrace with an unchanged image list re-read mapped headers: {line}"
        );
        assert!(cached.iter().any(|image| image.build_id.known().is_some()), "{cached:#?}");
        assert!(frames > 50, "{frames} frames");
    });
}
