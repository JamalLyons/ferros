//! Mach-O mapping and universal slice selection (#169, #170, #175), moved from the engine.

use std::fs;
use std::path::{Path, PathBuf};

use ferros_core::error::{DebuggerError, Result};
use ferros_core::objfile::{FunctionUnwind, ImageContents, SectionId, UnwindHint};
use ferros_core::symbols::{BinaryImage, ImageDescriptor, SymbolCache};
use ferros_core::types::{Address, Architecture};
use gimli::write::{Address as WriteAddress, AttributeValue, DwarfUnit, EndianVec, LineProgram, LineString, Sections};
use gimli::{DW_AT_high_pc, DW_AT_low_pc, DW_AT_name, DW_TAG_subprogram, Encoding, Format, LineEncoding, LittleEndian};

use super::*;
use crate::macho::{DebugInfoOptions, MachOFormat};

const ARM64: u32 = 0x0100_000c;
const X86_64: u32 = 0x0100_0007;
/// arm64e with the pointer-authentication ABI capability bit, as in `/usr/lib/dyld`.
const ARM64E: u32 = 0x8000_0002;

/// A little-endian 64-bit Mach-O with an `LC_UUID`, a `__TEXT` segment at `text`
/// covering the whole file, and three one-byte DWARF sections, so it parses with
/// embedded debug info and no tool.
fn slice(cpu_type: u32, cpu_subtype: u32, uuid: [u8; 16], text: u64) -> Vec<u8>
{
    let sections = [
        ("__debug_info", &[0][..]),
        ("__debug_abbrev", &[0][..]),
        ("__debug_line", &[0][..]),
    ];
    slice_with_sections(cpu_type, cpu_subtype, uuid, text, &sections)
}

fn slice_with_sections(cpu_type: u32, cpu_subtype: u32, uuid: [u8; 16], text: u64, sections: &[(&str, &[u8])]) -> Vec<u8>
{
    let dwarf_size = 72 + 80 * sections.len();
    let commands_size = 24 + 72 + dwarf_size;
    let mut data_offset = 32 + commands_size;
    let len = data_offset + sections.iter().map(|(_, data)| data.len()).sum::<usize>();
    let mut bytes = vec![0; len];
    let word = |bytes: &mut [u8], offset: usize, value: u32| bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    let quad = |bytes: &mut [u8], offset: usize, value: u64| bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    word(&mut bytes, 0, 0xfeed_facf);
    word(&mut bytes, 4, cpu_type);
    word(&mut bytes, 8, cpu_subtype);
    word(&mut bytes, 12, 2);
    word(&mut bytes, 16, 3);
    word(&mut bytes, 20, u32::try_from(commands_size).unwrap());
    word(&mut bytes, 32, 0x1b);
    word(&mut bytes, 36, 24);
    bytes[40..56].copy_from_slice(&uuid);
    let segment = 56;
    word(&mut bytes, segment, 0x19);
    word(&mut bytes, segment + 4, 72);
    bytes[segment + 8..segment + 14].copy_from_slice(b"__TEXT");
    quad(&mut bytes, segment + 24, text);
    quad(&mut bytes, segment + 32, 0x4000);
    quad(&mut bytes, segment + 48, len as u64);
    let segment = 128;
    word(&mut bytes, segment, 0x19);
    word(&mut bytes, segment + 4, u32::try_from(dwarf_size).unwrap());
    bytes[segment + 8..segment + 15].copy_from_slice(b"__DWARF");
    word(&mut bytes, segment + 64, u32::try_from(sections.len()).unwrap());
    for (index, &(name, data)) in sections.iter().enumerate() {
        let section = segment + 72 + 80 * index;
        bytes[section..section + name.len()].copy_from_slice(name.as_bytes());
        bytes[section + 16..section + 23].copy_from_slice(b"__DWARF");
        quad(&mut bytes, section + 40, data.len() as u64);
        word(&mut bytes, section + 48, u32::try_from(data_offset).unwrap());
        bytes[data_offset..data_offset + data.len()].copy_from_slice(data);
        data_offset += data.len();
    }
    bytes
}

/// A 32-bit universal file holding `slices`, each aligned to 2^12.
fn universal(slices: &[&[u8]], cpus: &[(u32, u32)]) -> Vec<u8>
{
    let mut fat = vec![0; 0x1000];
    fat[..4].copy_from_slice(&0xcafe_babe_u32.to_be_bytes());
    fat[4..8].copy_from_slice(&u32::try_from(slices.len()).unwrap().to_be_bytes());
    for (index, (data, &(cpu_type, cpu_subtype))) in slices.iter().zip(cpus).enumerate() {
        let offset = fat.len();
        let header = 8 + 20 * index;
        for (field, value) in [
            cpu_type,
            cpu_subtype,
            u32::try_from(offset).unwrap(),
            u32::try_from(data.len()).unwrap(),
            12,
        ]
        .into_iter()
        .enumerate()
        {
            fat[header + 4 * field..header + 4 * field + 4].copy_from_slice(&value.to_be_bytes());
        }
        fat.extend_from_slice(data);
        fat.resize(fat.len().next_multiple_of(0x1000), 0);
    }
    fat
}

fn write(name: &str, bytes: &[u8]) -> PathBuf
{
    let path = std::env::temp_dir().join(format!("ferros-macos-universal-{name}-{}", std::process::id()));
    fs::write(&path, bytes).unwrap();
    path
}

fn options() -> DebugInfoOptions
{
    DebugInfoOptions {
        dsymutil: None,
        ..DebugInfoOptions::default()
    }
}

fn cache() -> SymbolCache
{
    SymbolCache::new(MachOFormat::with_debug_info_options(options()).into_shared())
}

fn parse(path: &Path, target: Option<&SliceIdentity>) -> Result<ImageContents>
{
    open(path, 0x5_0000_0000, target, &options()).map(|(contents, _)| contents)
}

/// The link-time address of the parsed image's header: which slice was read.
fn text(image: &ImageContents) -> Option<u64>
{
    image.mapping.file_address(Address::new(0x5_0000_0000))
}

/// The link-time address of a loaded image's header.
fn loaded_text(image: &BinaryImage) -> Option<u64>
{
    image.file_address(Address::new(0x5_0000_0000))
}

/// DWARF 4 for one function `name` at `[entry, entry + 4)` with one line row, written
/// with `gimli::write`: `.debug_info`, `.debug_abbrev`, `.debug_line` and
/// `.debug_ranges`.
fn function_dwarf(name: &str, entry: u64) -> Vec<(SectionId, Vec<u8>)>
{
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };
    let mut dwarf = DwarfUnit::new(encoding);
    let mut program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(b"/src".to_vec()),
        None,
        LineString::String(b"example.rs".to_vec()),
        None,
    );
    let directory = program.default_directory();
    let file = program.add_file(LineString::String(b"example.rs".to_vec()), directory, None);
    program.begin_sequence(Some(WriteAddress::Constant(entry)));
    program.row().file = file;
    program.row().line = 1;
    program.generate_row();
    program.end_sequence(4);
    dwarf.unit.line_program = program;
    let root = dwarf.unit.root();
    let root_entry = dwarf.unit.get_mut(root);
    root_entry.set(DW_AT_name, AttributeValue::String(b"example.rs".to_vec()));
    root_entry.set(DW_AT_low_pc, AttributeValue::Address(WriteAddress::Constant(entry)));
    root_entry.set(DW_AT_high_pc, AttributeValue::Udata(4));
    let function = dwarf.unit.add(root, DW_TAG_subprogram);
    let function = dwarf.unit.get_mut(function);
    function.set(DW_AT_name, AttributeValue::String(name.as_bytes().to_vec()));
    function.set(DW_AT_low_pc, AttributeValue::Address(WriteAddress::Constant(entry)));
    function.set(DW_AT_high_pc, AttributeValue::Udata(4));
    let mut sections = Sections::new(EndianVec::new(LittleEndian));
    dwarf.write(&mut sections).expect("write DWARF");
    let mut out = Vec::new();
    sections
        .for_each(|id, data| {
            if !data.slice().is_empty() {
                out.push((id, data.slice().to_vec()));
            }
            Ok::<_, gimli::write::Error>(())
        })
        .expect("collect DWARF");
    out
}

#[test]
fn public_slice_selection_reads_matching_symbols_and_mapping()
{
    let with_function = |subtype, uuid, text, name: &str| {
        let dwarf = function_dwarf(name, text + 0x100);
        let names: Vec<String> = dwarf
            .iter()
            .map(|(id, _)| format!("__{}", id.name().trim_start_matches('.')))
            .collect();
        let sections: Vec<(&str, &[u8])> = names
            .iter()
            .zip(&dwarf)
            .map(|(name, (_, data))| (name.as_str(), data.as_slice()))
            .collect();
        slice_with_sections(ARM64, subtype, uuid, text, &sections)
    };
    let arm64 = with_function(0, [1; 16], 0x1_0000_0000, "plain_arm64");
    let arm64e = with_function(ARM64E, [2; 16], 0x2_0000_0000, "arm64e_only");
    let path = write("public-slice", &universal(&[&arm64, &arm64e], &[(ARM64, 0), (ARM64, ARM64E)]));
    let descriptor = ImageDescriptor {
        path: path.clone(),
        load_address: 0x5_0000_0000,
    };
    let mut cache = cache();
    let plain = cache.load_image_slice(descriptor.clone(), &SliceIdentity::new(ARM64, 0, None).to_image_slice());
    let selector = SliceIdentity::new(ARM64, 2, None);
    let arm64e_image = cache.load_image_slice(descriptor.clone(), &selector.to_image_slice());
    let mapping = slice_mapping(&descriptor.path, descriptor.load_address, &selector);
    let unselected = cache.load_image(descriptor);
    fs::remove_file(&path).unwrap();

    let runtime = Address::new(0x5_0000_0100);
    let plain = plain.expect("plain arm64 image");
    let arm64e_image = arm64e_image.expect("arm64e image");
    assert_ne!(plain.id(), arm64e_image.id());
    assert_eq!(plain.function_containing(runtime).unwrap().unwrap().name, "plain_arm64");
    assert_eq!(
        arm64e_image.function_containing(runtime).unwrap().unwrap().name,
        "arm64e_only"
    );
    assert_eq!(arm64e_image.file_address(runtime), Some(0x2_0000_0100));
    assert_eq!(mapping.unwrap().file_address(runtime), Some(0x2_0000_0100));
    assert!(matches!(unselected, Err(DebuggerError::NoDebugInfo { .. })));
}

#[test]
fn public_slice_selection_refuses_the_wrong_uuid()
{
    let arm64e = slice(ARM64, ARM64E, [1; 16], 0x2_0000_0000);
    let path = write("public-uuid", &universal(&[&arm64e], &[(ARM64, ARM64E)]));
    let descriptor = ImageDescriptor {
        path: path.clone(),
        load_address: 0x5_0000_0000,
    };
    let mut cache = cache();
    let correct = SliceIdentity::new(ARM64, 2, Some([1; 16]));
    let wrong = SliceIdentity::new(ARM64, 2, Some([2; 16]));
    assert!(cache.load_image_slice(descriptor.clone(), &correct.to_image_slice()).is_ok());
    let assert_refused = |cache: &mut SymbolCache| {
        for error in [
            cache.load_image_slice(descriptor.clone(), &wrong.to_image_slice()).err(),
            slice_mapping(&descriptor.path, descriptor.load_address, &wrong).err(),
        ] {
            let error = error.expect("another build");
            assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
            assert!(error.to_string().contains("another build"), "{error}");
        }
    };
    assert_refused(&mut cache);
    fs::write(&path, arm64e).unwrap();
    assert_refused(&mut cache);
    fs::remove_file(&path).unwrap();
}

#[test]
fn the_slice_the_target_mapped_is_parsed_by_cpu_type_and_subtype()
{
    let x86 = slice(X86_64, 3, [1; 16], 0x3_0000_0000);
    let arm64 = slice(ARM64, 0, [2; 16], 0x1_0000_0000);
    let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
    let path = write(
        "subtype",
        &universal(&[&x86, &arm64, &arm64e], &[(X86_64, 3), (ARM64, 0), (ARM64, ARM64E)]),
    );
    let mapped = SliceIdentity::from_header(&arm64e).unwrap();
    assert_eq!(
        mapped,
        SliceIdentity {
            cpu_type: ARM64,
            cpu_subtype: ARM64E,
            uuid: Some([3; 16]),
        }
    );
    let arm64e_image = parse(&path, Some(&mapped));
    // Capability bits are not part of the subtype: a header without them still names
    // the arm64e slice, never the plain arm64 one that shares its object::Architecture.
    let without_capabilities = SliceIdentity {
        cpu_subtype: 2,
        uuid: None,
        ..mapped
    };
    let arm64e_by_subtype = parse(&path, Some(&without_capabilities));
    let arm64_image = parse(&path, Some(&SliceIdentity::from_header(&arm64).unwrap()));
    let _ = fs::remove_file(&path);

    let arm64e_image = arm64e_image.expect("arm64e slice");
    assert_eq!(text(&arm64e_image), Some(0x2_0000_0000));
    assert_eq!(arm64e_image.architecture, Architecture::Arm64);
    assert_eq!(
        text(&arm64e_by_subtype.expect("arm64e slice by subtype")),
        Some(0x2_0000_0000)
    );
    assert_eq!(text(&arm64_image.expect("arm64 slice")), Some(0x1_0000_0000));
}

#[test]
fn the_uuid_chooses_between_slices_of_the_same_cpu()
{
    let first = slice(ARM64, ARM64E, [4; 16], 0x1_0000_0000);
    let second = slice(ARM64, ARM64E, [5; 16], 0x2_0000_0000);
    let bytes = universal(&[&first, &second], &[(ARM64, ARM64E), (ARM64, ARM64E)]);
    let path = Path::new("/universal");
    let target = SliceIdentity::from_header(&second).unwrap();
    assert_eq!(image_slice(path, &bytes, Some(&target)).unwrap(), &second[..]);
    let unknown_uuid = SliceIdentity { uuid: None, ..target };
    assert_eq!(image_slice(path, &bytes, Some(&unknown_uuid)).unwrap(), &first[..]);
    // A single image is read as it is when its UUID is the mapped one or unknown.
    assert_eq!(image_slice(path, &second, Some(&target)).unwrap(), &second[..]);
    assert_eq!(image_slice(path, &first, Some(&unknown_uuid)).unwrap(), &first[..]);
    assert_eq!(image_slice(path, &first, None).unwrap(), &first[..]);
}

/// The symbol cache keys a parse on the mapped UUID too: two builds at the same path
/// and load address get their own entries, and the second load never returns the
/// first's parse (#170).
#[test]
fn the_symbol_cache_keys_a_parse_on_the_mapped_uuid()
{
    let first = slice(ARM64, ARM64E, [8; 16], 0x1_0000_0000);
    let second = slice(ARM64, ARM64E, [9; 16], 0x2_0000_0000);
    let path = write(
        "cache-key",
        &universal(&[&first, &second], &[(ARM64, ARM64E), (ARM64, ARM64E)]),
    );
    let mut cache = cache();
    let descriptor = ImageDescriptor {
        path: path.clone(),
        load_address: 0x5_0000_0000,
    };
    let load = |cache: &mut SymbolCache, bytes: &[u8]| {
        let target = SliceIdentity::from_header(bytes).unwrap();
        cache.load_image_slice(descriptor.clone(), &target.to_image_slice())
    };
    let first_image = load(&mut cache, &first);
    let second_image = load(&mut cache, &second);
    let _ = fs::remove_file(&path);

    let (first_image, second_image) = (first_image.expect("first build"), second_image.expect("second build"));
    assert_ne!(first_image.id(), second_image.id());
    assert_eq!(loaded_text(&first_image), Some(0x1_0000_0000));
    assert_eq!(
        loaded_text(&second_image),
        Some(0x2_0000_0000),
        "the second build's own parse"
    );
}

/// A single-architecture file rebuilt while the old build stays mapped is another
/// build: a retryable failure, never a parse labelled with the mapped UUID (#170).
#[test]
fn a_replaced_single_image_is_another_build()
{
    let mapped = slice(ARM64, ARM64E, [6; 16], 0x1_0000_0000);
    let rebuilt = slice(ARM64, ARM64E, [7; 16], 0x1_0000_0000);
    let target = SliceIdentity::from_header(&mapped).unwrap();
    let path = write("replaced", &rebuilt);
    let parsed = parse(&path, Some(&target));
    let reparsed = parse(&path, Some(&SliceIdentity { uuid: None, ..target }));
    let _ = fs::remove_file(&path);
    let error = parsed.expect_err("another build");
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
    assert!(error.to_string().contains("another build"), "{error}");
    assert!(reparsed.is_ok(), "an unknown mapped UUID reads the file as it is");
}

#[test]
fn a_universal_file_without_the_target_slice_has_no_debug_info()
{
    let x86 = slice(X86_64, 3, [1; 16], 0x3_0000_0000);
    let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
    let path = write("missing", &universal(&[&x86, &arm64e], &[(X86_64, 3), (ARM64, ARM64E)]));
    let other_build = SliceIdentity {
        uuid: Some([9; 16]),
        ..SliceIdentity::from_header(&arm64e).unwrap()
    };
    let plain_arm64 = SliceIdentity {
        cpu_type: ARM64,
        cpu_subtype: 0,
        uuid: None,
    };
    let replaced = parse(&path, Some(&other_build)).err();
    let results = [
        parse(&path, Some(&plain_arm64)).err(),
        parse(&path, None).err(),
        cache()
            .load_image(ImageDescriptor {
                path: path.clone(),
                load_address: 0x5_0000_0000,
            })
            .err(),
    ];
    let mapping = file_mapping(&path, 0x5_0000_0000);
    let _ = fs::remove_file(&path);
    for error in results {
        let error = error.expect("no slice to read");
        assert!(
            matches!(&error, DebuggerError::NoDebugInfo { path: reported, .. } if *reported == path),
            "{error}"
        );
        assert!(error.to_string().contains("universal"), "{error}");
    }
    // A slice of the mapped CPU from another build means the file was replaced: a
    // load failure, retried once the file changes, not debug info that is missing.
    let error = replaced.expect("another build");
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
    assert!(error.to_string().contains("another build"), "{error}");
    let error = mapping.expect_err("no slice to map");
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
    assert!(error.to_string().contains("universal"), "{error}");
}

/// With the mapped UUID known, a candidate slice that is not a valid Mach-O is
/// reported as damaged, not as a build mismatch or missing debug info.
#[test]
fn a_damaged_candidate_slice_is_reported_as_damaged()
{
    let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
    let target = SliceIdentity::from_header(&arm64e).unwrap();
    let damaged = vec![0xab; arm64e.len()];
    let bytes = universal(&[&damaged], &[(ARM64, ARM64E)]);
    let error = image_slice(Path::new("/universal"), &bytes, Some(&target)).unwrap_err();
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
    assert!(!error.to_string().contains("another build"), "{error}");
}

#[test]
fn a_malformed_universal_file_is_a_parse_error()
{
    let arm64e = slice(ARM64, ARM64E, [3; 16], 0x2_0000_0000);
    let mut bytes = universal(&[&arm64e], &[(ARM64, ARM64E)]);
    bytes.truncate(0x1000 + arm64e.len() / 2);
    let target = SliceIdentity::from_header(&arm64e).unwrap();
    let error = image_slice(Path::new("/universal"), &bytes, Some(&target)).unwrap_err();
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
}

fn macho_segments() -> Vec<u8>
{
    let segments = [
        ("__PAGEZERO", 0_u64, 0x1_0000_0000_u64, 0_u64, 0_u64),
        ("__HEADER", 0x1_0000_0000, 0x1000, 0, 248),
        ("__DATA", 0x1_0000_2000, 0x1000, 0x1000, 0),
    ];
    let mut bytes = vec![0; 32 + 72 * 3];
    bytes[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&2_u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&3_u32.to_le_bytes());
    bytes[20..24].copy_from_slice(&216_u32.to_le_bytes());
    for (index, (name, address, size, file_offset, file_size)) in segments.into_iter().enumerate() {
        let command = &mut bytes[32 + 72 * index..32 + 72 * (index + 1)];
        command[0..4].copy_from_slice(&0x19_u32.to_le_bytes());
        command[4..8].copy_from_slice(&72_u32.to_le_bytes());
        command[8..8 + name.len()].copy_from_slice(name.as_bytes());
        command[24..32].copy_from_slice(&address.to_le_bytes());
        command[32..40].copy_from_slice(&size.to_le_bytes());
        command[40..48].copy_from_slice(&file_offset.to_le_bytes());
        command[48..56].copy_from_slice(&file_size.to_le_bytes());
    }
    bytes
}

#[test]
fn mapping_uses_header_segment_excludes_pagezero_and_rejects_overflow()
{
    let bytes = macho_segments();
    let file = object::File::parse(&*bytes).unwrap();
    let mapping = mapping_from_file(&file, 0x1000).unwrap();
    assert_eq!(mapping.file_address(Address::new(0x1020)), Some(0x1_0000_0020));
    assert_eq!(mapping.runtime_address(0x1_0000_2020), Some(Address::new(0x3020)));
    assert_eq!(mapping.runtime_address(0), None);
    assert_eq!(mapping.runtime_address(0x1_0000_1000), None);
    assert!(mapping_from_file(&file, u64::MAX - 0x100).is_err());
}

#[test]
fn mapped_header_gives_the_file_layout_and_rejects_bad_headers()
{
    let bytes = macho_segments();
    let file = object::File::parse(&*bytes).unwrap();
    assert_eq!(crate::macho::header_size(&bytes).unwrap(), bytes.len());
    assert_eq!(
        mapping_from_header(&bytes, 0x1000).unwrap(),
        mapping_from_file(&file, 0x1000).unwrap()
    );
    assert!(mapping_from_header(&bytes[..bytes.len() - 1], 0x1000).is_err());
    assert!(mapping_from_header(&bytes, u64::MAX - 0x100).is_err());
    assert!(crate::macho::header_size(&[0; 32]).is_err());
    assert!(crate::macho::header_size(&bytes[..16]).is_err());
    let mut huge = bytes.clone();
    huge[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(crate::macho::header_size(&huge).is_err());
}

/// The slice the session holds round-trips to the selector, with the UUID as a 16-byte
/// build id and without the subtype's capability bits in the cache key.
#[test]
fn a_slice_identity_round_trips_through_the_engine_slice()
{
    let arm64e = SliceIdentity::new(ARM64, ARM64E, Some([3; 16]));
    let slice = arm64e.to_image_slice();
    let back = SliceIdentity::from_image_slice(&slice).unwrap();
    assert!(back.names(ARM64, ARM64E));
    assert_eq!(back.uuid, Some([3; 16]));
    assert_eq!(
        SliceIdentity::new(ARM64, 2, Some([3; 16])).to_image_slice(),
        slice,
        "capability bits are not part of the key"
    );
    let unknown = SliceIdentity::new(ARM64, ARM64E, None).to_image_slice();
    assert_eq!(unknown.build_id(), ferros_core::types::BuildIdentity::Unknown);
    assert_eq!(
        SliceIdentity::from_image_slice(&unknown.or_later(slice.build_id()))
            .unwrap()
            .uuid,
        Some([3; 16]),
        "a UUID the loader reports later selects the slice"
    );
}

/// Set the link-time address of section `index` of a [`slice_with_sections`] image.
fn set_section_address(bytes: &mut [u8], index: usize, address: u64)
{
    let section = 128 + 72 + 80 * index;
    bytes[section + 32..section + 40].copy_from_slice(&address.to_le_bytes());
}

/// An image with `__unwind_info` (the compact unwind test table), `__eh_frame` and
/// `__eh_frame_hdr` hands the engine those sections and the table's hints, relative to
/// the load address.
#[test]
fn open_reads_unwind_info_and_eh_frame_sections()
{
    let unwind_info = crate::macho::compact_unwind::tests::section();
    // The table's DWARF-mode entry for 0x2000 names an FDE at 0x158: room for it.
    let eh_frame: Vec<u8> = (0..0x180_u32).map(|i| i as u8).collect();
    let eh_frame_hdr = [1_u8, 0x1b, 0x03, 0x3b, 9, 8, 7, 6];
    let sections = [
        ("__debug_info", &[0][..]),
        ("__debug_abbrev", &[0][..]),
        ("__debug_line", &[0][..]),
        ("__unwind_info", &unwind_info[..]),
        ("__eh_frame", &eh_frame[..]),
        ("__eh_frame_hdr", &eh_frame_hdr[..]),
    ];
    let mut bytes = slice_with_sections(ARM64, 0, [1; 16], 0x1_0000_0000, &sections);
    set_section_address(&mut bytes, 4, 0x1_0000_3000);
    set_section_address(&mut bytes, 5, 0x1_0000_3400);
    let path = write("unwind-sections", &bytes);
    let image = parse(&path, None);
    let _ = fs::remove_file(&path);
    let image = image.expect("image with unwind sections");

    let eh = image.eh_frame.as_ref().expect("__eh_frame");
    assert_eq!(eh.address, 0x1_0000_3000);
    assert_eq!(&*eh.data, &eh_frame[..]);
    let hdr = image.eh_frame_hdr.as_ref().expect("__eh_frame_hdr");
    assert_eq!(hdr.address, 0x1_0000_3400);
    assert_eq!(&*hdr.data, &eh_frame_hdr[..]);

    let hints = image.unwind_hints.as_deref().expect("__unwind_info hints");
    let hint = |offset: u64| hints.lookup(Address::new(0x5_0000_0000 + offset));
    assert_eq!(
        hint(0x1004),
        Some(FunctionUnwind {
            start: Address::new(0x5_0000_1000),
            hint: UnwindHint::FrameRecord,
        })
    );
    // 0x0200_1000: frameless, ((encoding >> 12) & 0xfff) * 16 bytes of stack.
    assert_eq!(
        hint(0x1044),
        Some(FunctionUnwind {
            start: Address::new(0x5_0000_1040),
            hint: UnwindHint::Leaf {
                stack_size: u64::from((0x0200_1000_u32 >> 12) & 0xFFF) * 16,
            },
        })
    );
    // 0x0300_0158: DWARF mode, FDE at 0x158 in __eh_frame.
    let cfi = hint(0x2abc).expect("DWARF-mode entry");
    assert_eq!(cfi.start, Address::new(0x5_0000_2000));
    assert_eq!(cfi.hint, UnwindHint::Cfi { eh_frame_offset: 0x158 });
    let UnwindHint::Cfi { eh_frame_offset } = cfi.hint else {
        unreachable!()
    };
    assert!(eh_frame_offset < eh.data.len(), "the FDE offset lies inside __eh_frame");
    // Outside the table: before its first function, at its sentinel, below the header.
    assert_eq!(hint(0x0fff), None);
    assert_eq!(hint(0x3000), None);
    assert_eq!(hints.lookup(Address::new(0x4_ffff_ffff)), None);
}

/// An entry with encoding 0 in the image's `__unwind_info` is no hint.
#[test]
fn open_gives_no_hint_for_an_entry_with_encoding_zero()
{
    let mut unwind_info = crate::macho::compact_unwind::tests::section();
    // The regular page's only entry (0x2000) is the table's last word.
    let last = unwind_info.len() - 4;
    unwind_info[last..].copy_from_slice(&0_u32.to_le_bytes());
    let sections = [
        ("__debug_info", &[0][..]),
        ("__debug_abbrev", &[0][..]),
        ("__debug_line", &[0][..]),
        ("__unwind_info", &unwind_info[..]),
    ];
    let path = write(
        "unwind-zero",
        &slice_with_sections(ARM64, 0, [1; 16], 0x1_0000_0000, &sections),
    );
    let image = parse(&path, None);
    let _ = fs::remove_file(&path);
    let image = image.expect("image with unwind info");
    let hints = image.unwind_hints.as_deref().expect("__unwind_info hints");
    assert_eq!(hints.lookup(Address::new(0x5_0000_2abc)), None);
    assert_eq!(
        hints.lookup(Address::new(0x5_0000_1004)).map(|entry| entry.hint),
        Some(UnwindHint::FrameRecord),
        "the other entries still give hints"
    );
}

/// Without `__unwind_info`, `__eh_frame` or `__eh_frame_hdr` the image has none of them.
#[test]
fn open_without_unwind_sections_has_no_hints_or_eh_frame()
{
    let path = write("no-unwind", &slice(ARM64, 0, [1; 16], 0x1_0000_0000));
    let image = parse(&path, None);
    let _ = fs::remove_file(&path);
    let image = image.expect("image without unwind sections");
    assert!(image.unwind_hints.is_none());
    assert!(image.eh_frame.is_none());
    assert!(image.eh_frame_hdr.is_none());
}

/// A file whose UUID cannot be read: its `LC_UUID` command is too short to hold a UUID
/// and its load command list ends early, so `mach_uuid` fails while the segments before
/// the break still parse.
fn slice_with_unreadable_uuid() -> Vec<u8>
{
    let mut bytes = slice(ARM64, ARM64E, [0; 16], 0x1_0000_0000);
    let word = |bytes: &mut [u8], offset: usize, value: u32| bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    // `LC_UUID` with `cmdsize` 16, then an empty 8-byte command in the rest of its 24
    // bytes, then the two segments, then a fifth command past `sizeofcmds`.
    word(&mut bytes, 16, 5);
    word(&mut bytes, 36, 16);
    word(&mut bytes, 48, 0);
    word(&mut bytes, 52, 8);
    bytes
}

/// #249: a file whose UUID command cannot be read is not the mapped build. Its build id is
/// `Absent`, which never matches a known mapped id, so a late id always contradicts it;
/// opened for a known mapped UUID it is refused as another build. While the mapped UUID is
/// unknown it is still read.
#[test]
fn an_unreadable_uuid_is_another_build_than_a_known_mapped_one()
{
    let bytes = slice_with_unreadable_uuid();
    let file = object::File::parse(&*bytes).expect("the file still parses");
    assert!(file.mach_uuid().is_err(), "the fixture's UUID must not be readable");
    let mapped = slice(ARM64, ARM64E, [4; 16], 0x1_0000_0000);
    let target = SliceIdentity::from_header(&mapped).unwrap();
    let path = write("unreadable-uuid", &bytes);
    let unknown = parse(&path, None);
    let known = parse(&path, Some(&target));
    let _ = fs::remove_file(&path);

    let unknown = unknown.expect("read while the mapped UUID is unknown");
    assert_eq!(unknown.build_id, ferros_core::types::BuildIdentity::Absent);
    assert_eq!(text(&unknown), Some(0x1_0000_0000));
    assert!(!ferros_core::types::same_build(
        unknown.build_id,
        target.to_image_slice().build_id()
    ));
    let error = known.expect_err("another build than the mapped UUID");
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
    assert!(error.to_string().contains("another build"), "{error}");
}

#[test]
fn a_known_mapped_uuid_skips_a_universal_slice_with_an_unreadable_uuid()
{
    let unreadable = slice_with_unreadable_uuid();
    let matching = slice(ARM64, ARM64E, [4; 16], 0x2_0000_0000);
    let target = SliceIdentity::from_header(&matching).unwrap();
    let path = Path::new("/universal");

    let bytes = universal(&[&unreadable, &matching], &[(ARM64, ARM64E), (ARM64, ARM64E)]);
    assert_eq!(image_slice(path, &bytes, Some(&target)).unwrap(), &matching[..]);

    let bytes = universal(&[&unreadable], &[(ARM64, ARM64E)]);
    let error = image_slice(path, &bytes, Some(&target)).expect_err("no slice with the mapped UUID");
    assert!(matches!(error, DebuggerError::InvalidArgument(_)), "{error}");
}

#[test]
fn an_unknown_mapped_uuid_serves_a_universal_slice_with_an_absent_build_id()
{
    let unreadable = slice_with_unreadable_uuid();
    let matching = slice(ARM64, ARM64E, [4; 16], 0x2_0000_0000);
    let known = SliceIdentity::from_header(&matching).unwrap();
    let unknown = SliceIdentity { uuid: None, ..known };
    let bytes = universal(&[&unreadable, &matching], &[(ARM64, ARM64E), (ARM64, ARM64E)]);
    let path = write("universal-unreadable-uuid", &bytes);
    let image = parse(&path, Some(&unknown));
    fs::remove_file(&path).unwrap();

    let image = image.expect("read the first slice while the mapped UUID is unknown");
    assert_eq!(text(&image), Some(0x1_0000_0000));
    assert_eq!(image.build_id, BuildIdentity::Absent);
    assert!(!ferros_core::types::same_build(
        image.build_id,
        known.to_image_slice().build_id()
    ));
}
