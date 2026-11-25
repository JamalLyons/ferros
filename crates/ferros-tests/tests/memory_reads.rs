//! Memory reads report the same error category on every read path (issue #216).
//!
//! The backend reads through its page cache (`Backend::read_memory`). The macOS helper
//! `ferros_macos::memory::read_memory`, reached here through `diagnostics::raw_memory`,
//! reads the same task without the cache. A read that runs from mapped bytes into a
//! hole used to fail as `NotFound` through the cache and, uncached, as an untyped I/O
//! error that the `Backend` boundary turned into `Os { code: -1 }`. Both must say the
//! bytes are not mapped, and mapped-but-unreadable bytes must be `PermissionDenied`
//! on both paths.
#![cfg(target_os = "macos")]

use ferros_core::{Address, BackendError, DebuggerError, MemoryRegion};
use ferros_macos::diagnostics;
use ferros_tests::fixture;
use ferros_tests::harness::{self, Debuggee};

/// Bytes read across each boundary: half before it, half after.
const SPAN: usize = 16;

/// The target's memory regions, sorted by start address.
fn regions(d: &mut Debuggee) -> Vec<MemoryRegion>
{
    let mut regions = d.session_mut().backend_mut().memory_regions().expect("memory regions");
    regions.sort_by_key(|region| region.start);
    regions
}

/// Whether `region` is readable.
fn readable(region: &MemoryRegion) -> bool
{
    region.permissions.starts_with('r')
}

/// The end of a readable region that is followed by an unmapped hole.
fn end_before_hole(regions: &[MemoryRegion]) -> Address
{
    regions
        .windows(2)
        .find(|pair| readable(&pair[0]) && pair[1].start.value() >= pair[0].end.value() + SPAN as u64)
        .map(|pair| pair[0].end)
        .expect("a readable region followed by an unmapped hole")
}

/// Read `SPAN` bytes at `at` through the backend's cache and directly, returning both errors.
fn read_both_ways(d: &mut Debuggee, at: Address) -> (BackendError, DebuggerError)
{
    let pid = d.pid();
    let cached = d
        .session_mut()
        .backend_mut()
        .read_memory(at, SPAN)
        .expect_err("cached read succeeded");
    let uncached = diagnostics::raw_memory(pid, at, SPAN).expect_err("uncached read succeeded");
    (cached, uncached)
}

#[test]
fn unmapped_read_is_not_found_on_both_paths()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch(&fixture);
        let end = end_before_hole(&regions(&mut d));

        // Starts mapped then runs into the hole, and starts in the hole. Each is read with
        // a cold cache, then again after a nearby successful read has filled the cache.
        for at in [Address::from(end.value() - SPAN as u64 / 2), end] {
            for warm in [false, true] {
                if warm {
                    let nearby = Address::from(end.value() - SPAN as u64);
                    d.session_mut()
                        .backend_mut()
                        .read_memory(nearby, SPAN)
                        .expect("nearby mapped bytes read through the cache");
                }
                let (cached, uncached) = read_both_ways(&mut d, at);
                assert!(
                    matches!(cached, BackendError::NotFound(_)),
                    "cached read at {at} (warm: {warm}): {cached:?}"
                );
                assert!(
                    matches!(uncached, DebuggerError::Backend(BackendError::NotFound(_))),
                    "uncached read at {at} (warm: {warm}): {uncached:?}"
                );
            }
        }
    });
}

#[test]
fn unreadable_read_is_permission_denied_on_both_paths()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch(&fixture);
        let regions = regions(&mut d);
        // A readable region directly followed by a mapped, unreadable one (a stack guard).
        let start = regions
            .windows(2)
            .find(|pair| readable(&pair[0]) && !readable(&pair[1]) && pair[0].end == pair[1].start)
            .map(|pair| pair[1].start)
            .or_else(|| regions.iter().find(|region| !readable(region)).map(|region| region.start))
            .expect("a mapped region that is not readable");

        let (cached, uncached) = read_both_ways(&mut d, start);
        assert!(
            matches!(cached, BackendError::PermissionDenied(_)),
            "cached read at {start}: {cached:?}"
        );
        assert!(
            matches!(uncached, DebuggerError::PermissionDenied(_)),
            "uncached read at {start}: {uncached:?}"
        );
    });
}
