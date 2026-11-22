//! Loader-reported image identity: the Mach-O UUID next to path and load address
//! (issue #121).
#![cfg(target_os = "macos")]

use std::fs;
use std::path::Path;
use std::time::Duration;

use ferros_core::{Address, BreakpointRequest, Session, StopEvent};
use ferros_tests::symbols::marker_address;
use ferros_tests::{fixture, harness};
use object::Object;

/// How long a test waits for one stop after resuming.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Resume until a breakpoint at `address` stops the process.
fn run_to(session: &mut Session, address: Address)
{
    session
        .add_breakpoint(BreakpointRequest::Software { address })
        .unwrap_or_else(|err| panic!("add_breakpoint at {address}: {err}"));
    session.resume().expect("resume");
    match session.wait(Some(STOP_TIMEOUT)).expect("wait") {
        Some(StopEvent::Breakpoint { pc, thread }) if pc == address => {
            session.select_thread(thread).expect("select the stopped thread");
        }
        other => panic!("expected a breakpoint stop at {address}, got {other:?}"),
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
        assert_eq!(main.uuid, Some(file_uuid(fixture.path())), "{main:?}");
        assert!(
            images.iter().any(|image| image.uuid.is_some()),
            "the non-cache executable must carry its UUID: {images:?}"
        );
    });
}
