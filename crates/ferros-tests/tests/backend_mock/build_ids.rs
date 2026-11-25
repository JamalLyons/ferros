//! Build identities decide whether a mapped image is reused.

use ferros_core::Session;
use ferros_core::backend::mock::{MockBackend, MockStop, arm64_registers};
use ferros_core::backend::{LaunchSpec, StopEvent};
use ferros_core::objfile::mock::{MockObjectFormat, MockObjectImage};
use ferros_core::types::Address;

use crate::helpers::*;

/// Load address of the image in the build-identity tests below.
const BUILD_IMAGE: u64 = 0x3_0000_0000;

/// A mapped image header in the neutral `MockObjectFormat` with one segment at file
/// address `text`, so a runtime address in the image maps back to an offset from `text`.
fn build_image_header(text: u64) -> Vec<u8>
{
    let mut header = MockObjectImage::new(0..1, 0, 0).with_mapping(text, 0x1000).header_bytes();
    header.resize(0x100, 0);
    header
}

fn build_image(build_id: ferros_core::BuildIdentity) -> ferros_core::backend::LoadedImage
{
    ferros_core::backend::LoadedImage {
        path: "/nonexistent/ferros-build-id-image".into(),
        load_address: Address::new(BUILD_IMAGE),
        build_id,
        role: ferros_core::backend::ImageRole::MainExecutable,
    }
}

/// A session over an image whose mapped header puts its segment at `0x1000`, reported with
/// `build_id`.
fn build_id_session(build_id: ferros_core::BuildIdentity) -> (MockBackend, Session)
{
    let mock = MockBackend::new()
        .with_thread(THREAD, arm64_registers())
        .with_region(Address::new(BUILD_IMAGE), "r-x", build_image_header(0x1000))
        .with_image(build_image(build_id));
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    (mock, session)
}

/// The file address the session maps the image's first code byte past its header to.
/// It follows the mapped header only when the session reads the image again.
fn mapped_text(session: &mut Session) -> Option<u64>
{
    session
        .file_address(Address::new(BUILD_IMAGE + 0x10))
        .expect("file address")
        .map(|(_, file)| file)
}

/// A 20-byte id (an ELF GNU build-id) whose last byte changes is another build at the same
/// path and address: the session reads its mapped header again, and refuses to map file
/// addresses of the old build.
#[test]
fn a_twenty_byte_build_id_that_changes_its_last_byte_is_a_new_build()
{
    let first = [0x5A; 20];
    let mut second = first;
    second[19] ^= 1;
    let (mock, mut session) = build_id_session(known(&first));
    assert_eq!(mapped_text(&mut session), Some(0x1010));

    mock.overwrite_memory(Address::new(BUILD_IMAGE), &build_image_header(0x8000));
    mock.set_images(vec![build_image(known(&second))]);
    assert_eq!(mapped_text(&mut session), Some(0x8010), "the replaced build's header is read");
    assert_eq!(
        session
            .runtime_address(&build_image(known(&first)), 0x8010)
            .expect("runtime address"),
        None,
        "file addresses of the old build do not apply"
    );
    assert_eq!(
        session
            .runtime_address(&build_image(known(&second)), 0x8010)
            .expect("runtime address"),
        Some(Address::new(BUILD_IMAGE + 0x10))
    );
}

/// An image whose format records no build id is never assumed to be the same build
/// across stops: with the id absent at both stops, the session reads the mapped header
/// again after the process ran. Within one stop the image is reused, and
/// `runtime_address` accepts the image it was handed (#193).
#[test]
fn an_absent_build_id_at_both_stops_is_not_the_same_build()
{
    let absent = ferros_core::BuildIdentity::Absent;
    let (mock, mut session) = build_id_session(absent);
    assert_eq!(mapped_text(&mut session), Some(0x1010));
    assert_eq!(
        session
            .runtime_address(&build_image(absent), 0x1010)
            .expect("runtime address"),
        Some(Address::new(BUILD_IMAGE + 0x10)),
        "within one stop the image is the one just read"
    );

    // Same stop: memory changes under the session are not re-read.
    mock.overwrite_memory(Address::new(BUILD_IMAGE), &build_image_header(0x8000));
    assert_eq!(mapped_text(&mut session), Some(0x1010), "the image is reused within a stop");

    mock.script(MockStop::new(StopEvent::Breakpoint {
        thread: THREAD,
        pc: Address::new(BUILD_IMAGE + 0x10),
    }));
    session.resume().expect("resume");
    session.wait(None).expect("wait");
    mock.set_images(vec![build_image(absent)]);
    assert_eq!(
        mapped_text(&mut session),
        Some(0x8010),
        "after a stop the image is read again"
    );
}

/// #170: an id the backend could not read at one stop and reads at the next is the same
/// build, so the session keeps the mapping it read; the header is not read again.
#[test]
fn an_unknown_build_id_that_becomes_known_keeps_the_mapping()
{
    let id = [0xC3; 16];
    let (mock, mut session) = build_id_session(ferros_core::BuildIdentity::Unknown);
    assert_eq!(mapped_text(&mut session), Some(0x1010));

    mock.overwrite_memory(Address::new(BUILD_IMAGE), &build_image_header(0x8000));
    mock.set_images(vec![build_image(known(&id))]);
    assert_eq!(mapped_text(&mut session), Some(0x1010), "the mapping read earlier is kept");
    assert_eq!(
        session
            .runtime_address(&build_image(known(&id)), 0x1010)
            .expect("runtime address"),
        Some(Address::new(BUILD_IMAGE + 0x10))
    );
}
