//! Public slice selection for universal images (#175).
#![cfg(target_os = "macos")]

use std::fs;

use ferros_core::Address;
use ferros_core::symbols::{ImageDescriptor, ImageMapping, SliceIdentity};
use object::macho::{CPU_SUBTYPE_ARM64E, CPU_TYPE_ARM64};
use object::read::macho::{FatArch, MachOFatFile32};
use object::{Object, ObjectSegment};

// This test only reads dyld's file on disk, so it needs no process harness.
#[test]
fn maps_the_selected_arm64e_slice_of_dyld()
{
    let descriptor = ImageDescriptor {
        path: "/usr/lib/dyld".into(),
        load_address: 0x5_0000_0000,
    };
    let bytes = fs::read(&descriptor.path).expect("read dyld");
    let fat = MachOFatFile32::parse(bytes.as_slice()).expect("dyld is universal");
    let arch = fat
        .arches()
        .iter()
        .find(|arch| arch.cputype() == CPU_TYPE_ARM64 && arch.cpusubtype().id() == CPU_SUBTYPE_ARM64E)
        .expect("dyld has an arm64e slice");
    let file = object::File::parse(arch.data(bytes.as_slice()).expect("arm64e slice bytes")).expect("arm64e Mach-O");
    let uuid = file.mach_uuid().expect("read arm64e UUID").expect("arm64e UUID");
    let selector = SliceIdentity::new(CPU_TYPE_ARM64.0, CPU_SUBTYPE_ARM64E.0, Some(uuid));
    let mapping = ImageMapping::parse_slice(&descriptor, &selector).expect("map the selected dyld slice");
    let text = file
        .segments()
        .find(|segment| segment.name().expect("segment name") == Some("__TEXT"))
        .expect("dyld's __TEXT segment");

    for segment in file
        .segments()
        .filter(|segment| segment.size() > 0 && segment.name().expect("segment name") != Some("__PAGEZERO"))
    {
        let start = segment.address();
        for address in [start, start + segment.size() - 1] {
            let runtime = Address::new(descriptor.load_address + (address - text.address()));
            assert_eq!(mapping.runtime_address(address), Some(runtime));
            assert_eq!(mapping.file_address(runtime), Some(address));
        }
    }
}
