//! Public variable inspection over synthetic DWARF and a stopped mock process.

use std::io::Write as _;

use ferros_core::backend::mock::{MockBackend, arm64_registers};
use ferros_core::backend::{ImageRole, LaunchSpec, LoadedImage};
use ferros_core::error::DebuggerError;
use ferros_core::objfile::mock::{MockObjectFile, MockObjectFormat, MockObjectImage};
use ferros_core::symbols::ImageDescriptor;
use ferros_core::types::{Address, BuildIdentity, FrameKind};
use ferros_core::values::format::{FormatOptions, ValueFormatter, ValueView};
use ferros_core::values::location::{Unavailable, ValueLocation};
use ferros_core::values::pretty::PrettyPrinter;
use ferros_core::values::types::{Type, TypeKind, TypeTable};
use ferros_core::{Session, SymbolCache};
use gimli::write::{
    Address as DwarfAddress, AttributeValue as Attr, DwarfUnit, EndianVec, Expression, Range, RangeList, Sections, Unit,
    UnitEntryId, Writer as _,
};
use gimli::{DwTag, Encoding, Format, LittleEndian, RunTimeEndian, SectionId};

use crate::helpers::{RemoveOnDrop, THREAD};

const BASE: u64 = 0x1_0000_0000;
const PC: u64 = BASE + 0x108;
const IMAGE_SIZE: u64 = 0x1000;

fn session(dwarf: &mut DwarfUnit, omit: Option<SectionId>) -> (Session, RemoveOnDrop)
{
    let mut sections = Sections::new(EndianVec::new(LittleEndian));
    dwarf.write(&mut sections).expect("write DWARF");
    let mut image = MockObjectFile::new(BASE, IMAGE_SIZE);
    for &id in ferros_core::objfile::DWARF_SECTIONS {
        if Some(id) != omit
            && let Some(data) = sections.get(id).filter(|data| !data.slice().is_empty())
        {
            image = image.with_dwarf(id, data.slice());
        }
    }
    let (mut file, path) = ferros_tests::scratch::create_file("ferros-mock-values", ".mock");
    file.write_all(&image.to_bytes()).expect("write mock image");
    drop(file);
    let cleanup = RemoveOnDrop(vec![path.clone()]);

    let base = Address::new(BASE);
    let mut bytes = MockObjectImage::new(0..1, 0, 0).with_mapping(BASE, IMAGE_SIZE).header_bytes();
    bytes.resize(usize::try_from(IMAGE_SIZE).unwrap(), 0);
    let mut registers = arm64_registers();
    registers.pc = Address::new(PC);
    registers.general[0] = 7;
    let backend = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(base, "r-x", bytes)
        .with_image(LoadedImage {
            path,
            load_address: base,
            build_id: BuildIdentity::Unknown,
            role: ImageRole::MainExecutable,
        });
    let mut session = Session::new(Box::new(backend), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("mock launch");
    (session, cleanup)
}

fn dwarf_function(name: Option<&str>) -> (DwarfUnit, UnitEntryId, UnitEntryId)
{
    let mut dwarf = DwarfUnit::new(Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    });
    let unit = &mut dwarf.unit;
    let root = unit.root();
    unit.get_mut(root)
        .set(gimli::DW_AT_language, Attr::Language(gimli::DW_LANG_Rust));
    unit.get_mut(root)
        .set(gimli::DW_AT_low_pc, Attr::Address(DwarfAddress::Constant(BASE + 0x100)));
    unit.get_mut(root).set(gimli::DW_AT_high_pc, Attr::Udata(0x30));
    let function = unit.add(root, gimli::DW_TAG_subprogram);
    if let Some(name) = name {
        unit.get_mut(function)
            .set(gimli::DW_AT_name, Attr::String(name.as_bytes().to_vec()));
    }
    unit.get_mut(function)
        .set(gimli::DW_AT_low_pc, Attr::Address(DwarfAddress::Constant(BASE + 0x100)));
    unit.get_mut(function).set(gimli::DW_AT_high_pc, Attr::Udata(0x20));
    let byte = unit.add(root, gimli::DW_TAG_base_type);
    unit.get_mut(byte).set(gimli::DW_AT_name, Attr::String(b"u8".to_vec()));
    unit.get_mut(byte).set(gimli::DW_AT_byte_size, Attr::Udata(1));
    (dwarf, function, byte)
}

fn variable(unit: &mut Unit, parent: UnitEntryId, tag: DwTag, name: Attr, ty: UnitEntryId) -> UnitEntryId
{
    let id = unit.add(parent, tag);
    unit.get_mut(id).set(gimli::DW_AT_name, name);
    unit.get_mut(id).set(gimli::DW_AT_type, Attr::UnitRef(ty));
    id
}

fn local(unit: &mut Unit, parent: UnitEntryId, name: &str, ty: UnitEntryId, expression: Expression)
{
    let id = variable(
        unit,
        parent,
        gimli::DW_TAG_variable,
        Attr::String(name.as_bytes().to_vec()),
        ty,
    );
    unit.get_mut(id).set(gimli::DW_AT_location, Attr::Exprloc(expression));
}

fn register_location() -> Expression
{
    let mut expression = Expression::new();
    expression.op_reg(gimli::Register(0));
    expression
}

fn index_type(unit: &mut Unit, encoding: gimli::DwAte, byte_size: u64) -> UnitEntryId
{
    let ty = unit.add(unit.root(), gimli::DW_TAG_base_type);
    unit.get_mut(ty).set(gimli::DW_AT_encoding, Attr::Encoding(encoding));
    unit.get_mut(ty).set(gimli::DW_AT_byte_size, Attr::Udata(byte_size));
    ty
}

fn array_with_bounds(
    unit: &mut Unit,
    element: UnitEntryId,
    index: Option<UnitEntryId>,
    lower: Attr,
    upper: Attr,
) -> UnitEntryId
{
    let array = unit.add(unit.root(), gimli::DW_TAG_array_type);
    unit.get_mut(array).set(gimli::DW_AT_type, Attr::UnitRef(element));
    let range = unit.add(array, gimli::DW_TAG_subrange_type);
    let entry = unit.get_mut(range);
    if let Some(index) = index {
        entry.set(gimli::DW_AT_type, Attr::UnitRef(index));
    }
    entry.set(gimli::DW_AT_lower_bound, lower);
    entry.set(gimli::DW_AT_upper_bound, upper);
    array
}

#[test]
fn oversized_piece_is_unavailable_through_session()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let mut ops = EndianVec::new(LittleEndian);
    ops.write_u8(gimli::DW_OP_reg0.0).unwrap();
    ops.write_u8(gimli::DW_OP_piece.0).unwrap();
    ops.write_uleb128(u64::MAX / 8 + 1).unwrap();
    local(&mut dwarf.unit, function, "bad_piece", byte, Expression::raw(ops.into_vec()));

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("locations fail per variable");
    assert_eq!(vars[0].name, "bad_piece");
    assert_eq!(
        vars[0].value.bytes,
        Err(Unavailable::InvalidDwarf(gimli::Error::InvalidPiece))
    );
    let (_, text) = session.format_variable(&vars[0], FormatOptions::default());
    assert_eq!(text, format!("<unavailable: {}>", vars[0].value.bytes.as_ref().unwrap_err()));
}

#[test]
fn bad_variable_name_leaves_its_neighbors_readable()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let bad_name = dwarf.strings.add(b"missing debug string".to_vec());
    local(&mut dwarf.unit, function, "first", byte, register_location());
    variable(
        &mut dwarf.unit,
        function,
        gimli::DW_TAG_variable,
        Attr::StringRef(bad_name),
        byte,
    );
    local(&mut dwarf.unit, function, "last", byte, register_location());

    let (mut session, _image) = session(&mut dwarf, Some(SectionId::DebugStr));
    let vars = session.frame_variables(0).expect("read remaining variables");
    assert_eq!(
        vars.iter().map(|var| var.name.as_str()).collect::<Vec<_>>(),
        ["first", "last"]
    );
    assert!(vars.iter().all(|var| var.value.bytes.as_deref() == Ok(&[7][..])));
}

fn add_inline(unit: &mut Unit, owner: UnitEntryId, byte: UnitEntryId) -> UnitEntryId
{
    local(unit, owner, "outer", byte, register_location());
    let abstract_fn = unit.add(unit.root(), gimli::DW_TAG_subprogram);
    unit.get_mut(abstract_fn)
        .set(gimli::DW_AT_name, Attr::String(b"inlined".to_vec()));
    let inline = unit.add(owner, gimli::DW_TAG_inlined_subroutine);
    let entry = unit.get_mut(inline);
    entry.set(gimli::DW_AT_abstract_origin, Attr::UnitRef(abstract_fn));
    entry.set(gimli::DW_AT_low_pc, Attr::Address(DwarfAddress::Constant(BASE + 0x104)));
    entry.set(gimli::DW_AT_high_pc, Attr::Udata(0x10));
    local(unit, inline, "inner", byte, register_location());
    abstract_fn
}

#[test]
fn selected_named_inline_under_unnamed_owner_includes_abstract_parameter()
{
    let (mut dwarf, owner, byte) = dwarf_function(None);
    let abstract_fn = add_inline(&mut dwarf.unit, owner, byte);
    variable(
        &mut dwarf.unit,
        abstract_fn,
        gimli::DW_TAG_formal_parameter,
        Attr::String(b"omitted".to_vec()),
        byte,
    );

    let (mut session, _image) = session(&mut dwarf, None);
    let selected = session.selected_stack_frame().expect("frame selected by backtrace");
    assert_eq!(selected.kind, FrameKind::Physical);
    assert!(selected.symbol.is_some_and(|name| name.raw() == "inlined"));
    let vars = session.selected_frame_variables().expect("locals of the selected frame");
    assert_eq!(
        vars.iter().map(|var| var.name.as_str()).collect::<Vec<_>>(),
        ["omitted", "inner"]
    );
    assert_eq!(vars[0].value.bytes, Err(Unavailable::OptimizedOut));
    assert_eq!(vars[1].value.bytes.as_deref(), Ok(&[7][..]));
}

#[test]
fn abstract_parameter_without_a_concrete_die_retains_its_constant()
{
    let (mut dwarf, owner, byte) = dwarf_function(None);
    let abstract_fn = add_inline(&mut dwarf.unit, owner, byte);
    let argument = variable(
        &mut dwarf.unit,
        abstract_fn,
        gimli::DW_TAG_formal_parameter,
        Attr::String(b"constant".to_vec()),
        byte,
    );
    dwarf.unit.get_mut(argument).set(gimli::DW_AT_const_value, Attr::Udata(42));

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.frame_variables(0).expect("locals of the selected frame");
    assert_eq!(vars[0].name, "constant");
    assert_eq!(vars[0].value.location, Some(ValueLocation::Implicit));
    assert_eq!(vars[0].value.bytes.as_deref(), Ok(&[42][..]));
}

#[test]
fn malformed_range_on_selected_scope_reports_no_debug_info()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    // addr2line can select this DIE by its valid range list. gimli rejects the
    // malformed low_pc encountered before that list when validating the scope.
    let range = dwarf
        .unit
        .ranges
        .add(RangeList(vec![Range::OffsetPair { begin: 0, end: 0x20 }]));
    dwarf
        .unit
        .get_mut(function)
        .set(gimli::DW_AT_low_pc, Attr::String(b"invalid address".to_vec()));
    dwarf
        .unit
        .get_mut(function)
        .set(gimli::DW_AT_ranges, Attr::RangeListRef(range));
    local(&mut dwarf.unit, function, "local", byte, register_location());

    let (mut session, _image) = session(&mut dwarf, None);
    let selected = session.selected_stack_frame().expect("addr2line selected the function");
    assert!(selected.symbol.is_some_and(|name| name.raw() == "function"));
    assert!(
        matches!(session.selected_frame_variables(), Err(DebuggerError::NoDebugInfo { detail, .. })
        if detail.contains("failed to walk variable DWARF"))
    );
}

#[test]
fn expression_can_skip_unparseable_bytes()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let ops = vec![gimli::DW_OP_skip.0, 1, 0, 0xff, gimli::DW_OP_reg0.0];
    local(&mut dwarf.unit, function, "skipped", byte, Expression::raw(ops));

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("inspect locals");
    assert_eq!(vars[0].value.location, Some(ValueLocation::Register(0)));
    assert_eq!(vars[0].value.bytes.as_deref(), Ok(&[7][..]));
}

#[test]
fn array_bounds_use_signed_sentinels_and_apply_element_stride()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let signed_byte_index = index_type(unit, gimli::DW_ATE_signed, 1);
    let signed_word_index = index_type(unit, gimli::DW_ATE_signed, 2);
    let bounds = vec![
        (
            "byte_stride",
            Some(signed_byte_index),
            Attr::Data1(u8::MAX - 1),
            Attr::Data1(0),
            Some((gimli::DW_AT_byte_stride, 2)),
        ),
        (
            "bit_stride",
            Some(signed_word_index),
            Attr::Data2(u16::MAX),
            Attr::Data2(1),
            Some((gimli::DW_AT_bit_stride, 16)),
        ),
        ("empty", None, Attr::Data4(0), Attr::Data4(u32::MAX), None),
        (
            "packed",
            None,
            Attr::Data1(0),
            Attr::Data1(1),
            Some((gimli::DW_AT_bit_stride, 7)),
        ),
    ];
    for (name, index, lower, upper, stride) in bounds {
        let array = array_with_bounds(unit, byte, index, lower, upper);
        if let Some((attribute, value)) = stride {
            unit.get_mut(array).set(attribute, Attr::Udata(value));
        }
        local(unit, function, name, array, register_location());
    }

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("array locals");
    assert_eq!(vars[0].value.bytes.as_deref(), Ok(&[7, 0, 0, 0, 0, 0][..]));
    assert_eq!(vars[1].value.bytes.as_deref(), Ok(&[7, 0, 0, 0, 0, 0][..]));
    assert_eq!(vars[2].value.bytes.as_deref(), Ok(&[][..]));
    assert_eq!(vars[3].value.bytes, Err(Unavailable::SizeUnknown));
}

#[test]
fn all_ones_byte_upper_bound_uses_index_signedness()
{
    const CASES: &[(&str, Option<gimli::DwAte>)] = &[
        ("unsigned", Some(gimli::DW_ATE_unsigned)),
        ("signed", Some(gimli::DW_ATE_signed)),
        ("untyped", None),
    ];
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let contents = (0..=u8::MAX).collect::<Vec<_>>();
    let unit = &mut dwarf.unit;
    for &(name, encoding) in CASES {
        let index = encoding.map(|encoding| index_type(unit, encoding, 1));
        let array = array_with_bounds(unit, byte, index, Attr::Data1(0), Attr::Data1(u8::MAX));
        let mut location = Expression::new();
        location.op_implicit_value(contents.clone().into_boxed_slice());
        local(unit, function, name, array, location);
    }

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("array locals");
    assert_eq!(vars[0].name, "unsigned");
    assert_eq!(vars[0].value.bytes.as_ref().map(Vec::len), Ok(256));
    assert_eq!(vars[0].value.bytes.as_deref(), Ok(contents.as_slice()));
    assert_eq!(vars[1].name, "signed");
    assert_eq!(vars[1].value.bytes.as_deref(), Ok(&[][..]));
    assert_eq!(vars[2].name, "untyped");
    assert_eq!(vars[2].value.bytes.as_deref(), Ok(contents.as_slice()));
}

#[test]
fn formatting_cuts_cycles_and_lets_a_printer_recurse()
{
    struct FirstByte;

    impl PrettyPrinter for FirstByte
    {
        fn matches(&self, ty: &Type, _: &TypeTable) -> bool
        {
            matches!(ty.kind, TypeKind::Struct(_)) && ty.name.as_deref() == Some("Recursive")
        }

        fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String
        {
            let byte = formatter.types().type_named("u8").expect("byte type");
            let first = formatter.format(value.slice(byte, 0, 1).expect("first byte"));
            format!("first byte is {first}")
        }
    }

    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    unit.get_mut(byte)
        .set(gimli::DW_AT_encoding, Attr::Encoding(gimli::DW_ATE_unsigned));
    let recursive = unit.add(unit.root(), gimli::DW_TAG_structure_type);
    unit.get_mut(recursive)
        .set(gimli::DW_AT_name, Attr::String(b"Recursive".to_vec()));
    unit.get_mut(recursive).set(gimli::DW_AT_byte_size, Attr::Udata(1));
    let child = variable(
        unit,
        recursive,
        gimli::DW_TAG_member,
        Attr::String(b"child".to_vec()),
        recursive,
    );
    unit.get_mut(child).set(gimli::DW_AT_data_member_location, Attr::Udata(0));
    let address = Address::new(BASE + 0x200);
    let mut location = Expression::new();
    location.op_addr(DwarfAddress::Constant(address.value()));
    local(unit, function, "recursive", recursive, location);

    let (mut session, image_path) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("locals");
    let options = FormatOptions::default();
    assert_eq!(session.format_variable(&vars[0], options).1, "Recursive { child: <cycle> }");
    let shallow = FormatOptions { max_depth: 1, ..options };
    assert_eq!(session.format_variable(&vars[0], shallow).1, "Recursive { ... }");

    let image = SymbolCache::new(MockObjectFormat::shared())
        .load_image(ImageDescriptor {
            path: image_path.0[0].clone(),
            load_address: BASE,
        })
        .expect("image with debug info");
    let types = image.types().expect("type table");
    let ty = types.type_named("Recursive").expect("recursive type");
    let mut read = |address, len| session.read_memory(address, len);
    let printers: &[&dyn PrettyPrinter] = &[&FirstByte];
    let mut formatter = ValueFormatter::new(types, RunTimeEndian::Little, options, &mut read).with_printers(printers);
    assert_eq!(formatter.format_at(ty, address), "first byte is 0");
    assert_eq!(
        formatter.format(ValueView {
            ty,
            bytes: &[37],
            address: None
        }),
        "first byte is 37"
    );
}

#[test]
fn unavailable_pointee_preserves_the_rest_of_the_value()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    unit.get_mut(byte)
        .set(gimli::DW_AT_encoding, Attr::Encoding(gimli::DW_ATE_unsigned));
    let reference = variable(
        unit,
        unit.root(),
        gimli::DW_TAG_pointer_type,
        Attr::String(b"&u8".to_vec()),
        byte,
    );
    let record = unit.add(unit.root(), gimli::DW_TAG_structure_type);
    unit.get_mut(record).set(gimli::DW_AT_name, Attr::String(b"Record".to_vec()));
    unit.get_mut(record).set(gimli::DW_AT_byte_size, Attr::Udata(16));
    let mut add_field = |name: &str, ty, offset| {
        let field = variable(unit, record, gimli::DW_TAG_member, Attr::String(name.as_bytes().to_vec()), ty);
        unit.get_mut(field)
            .set(gimli::DW_AT_data_member_location, Attr::Udata(offset));
    };
    add_field("reference", reference, 0);
    add_field("byte", byte, 8);
    let address = Address::new(BASE + 0x200);
    let mut location = Expression::new();
    location.op_addr(DwarfAddress::Constant(address.value()));
    local(unit, function, "record", record, location);

    let (mut session, _image) = session(&mut dwarf, None);
    let unmapped = BASE + IMAGE_SIZE;
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&unmapped.to_le_bytes());
    bytes[8] = 42;
    session.write_memory(address, &bytes).expect("store mock record");
    let vars = session.selected_frame_variables().expect("record bytes");
    let text = session.format_variable(&vars[0], FormatOptions::default()).1;
    assert!(
        text.starts_with(&format!("Record {{ reference: {unmapped:#x} -> <unavailable: ")),
        "{text}"
    );
    assert!(text.ends_with(">, byte: 42 }"), "{text}");
}

#[test]
fn unknown_standard_library_layouts_show_raw_fields()
{
    const NAMES: &[&str] = &["&str", "alloc::vec::Vec<u8, alloc::alloc::Global>"];
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    unit.get_mut(byte)
        .set(gimli::DW_AT_encoding, Attr::Encoding(gimli::DW_ATE_unsigned));
    for name in NAMES {
        let structure = unit.add(unit.root(), gimli::DW_TAG_structure_type);
        unit.get_mut(structure)
            .set(gimli::DW_AT_name, Attr::String(name.as_bytes().to_vec()));
        unit.get_mut(structure).set(gimli::DW_AT_byte_size, Attr::Udata(1));
        let field = variable(unit, structure, gimli::DW_TAG_member, Attr::String(b"unknown".to_vec()), byte);
        unit.get_mut(field).set(gimli::DW_AT_data_member_location, Attr::Udata(0));
        local(unit, function, name, structure, register_location());
    }

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("mock values");
    for name in NAMES {
        let var = vars.iter().find(|var| var.name == *name).expect(name);
        assert_eq!(
            session.format_variable(var, FormatOptions::default()).1,
            format!("{name} {{ unknown: 7 }} <unrecognized layout>")
        );
    }
}
