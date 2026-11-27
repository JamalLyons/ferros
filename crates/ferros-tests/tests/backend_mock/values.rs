//! Public variable inspection over synthetic DWARF and a stopped mock process.

use std::io::Write as _;

use ferros_core::Session;
use ferros_core::backend::mock::{MockBackend, arm64_registers};
use ferros_core::backend::{ImageRole, LaunchSpec, LoadedImage};
use ferros_core::error::DebuggerError;
use ferros_core::objfile::mock::{MockObjectFile, MockObjectFormat, MockObjectImage};
use ferros_core::types::{Address, BuildIdentity, FrameKind};
use ferros_core::values::location::{Unavailable, ValueLocation};
use gimli::write::{
    Address as DwarfAddress, AttributeValue as Attr, DwarfUnit, EndianVec, Expression, Range, RangeList, Sections, Unit,
    UnitEntryId, Writer as _,
};
use gimli::{DwTag, Encoding, Format, LittleEndian, SectionId};

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
    let bounds = vec![
        (
            "byte_stride",
            Attr::Data1(u8::MAX),
            Attr::Data1(1),
            Some((gimli::DW_AT_byte_stride, 2)),
        ),
        (
            "bit_stride",
            Attr::Data2(u16::MAX),
            Attr::Data2(1),
            Some((gimli::DW_AT_bit_stride, 16)),
        ),
        ("empty", Attr::Data4(0), Attr::Data4(u32::MAX), None),
        ("packed", Attr::Data1(0), Attr::Data1(1), Some((gimli::DW_AT_bit_stride, 7))),
    ];
    for (name, lower, upper, stride) in bounds {
        let array = unit.add(unit.root(), gimli::DW_TAG_array_type);
        unit.get_mut(array).set(gimli::DW_AT_type, Attr::UnitRef(byte));
        if let Some((attribute, value)) = stride {
            unit.get_mut(array).set(attribute, Attr::Udata(value));
        }
        let subrange = unit.add(array, gimli::DW_TAG_subrange_type);
        unit.get_mut(subrange).set(gimli::DW_AT_lower_bound, lower);
        unit.get_mut(subrange).set(gimli::DW_AT_upper_bound, upper);
        local(unit, function, name, array, register_location());
    }

    let (mut session, _image) = session(&mut dwarf, None);
    let vars = session.selected_frame_variables().expect("array locals");
    assert_eq!(vars[0].value.bytes.as_deref(), Ok(&[7, 0, 0, 0, 0, 0][..]));
    assert_eq!(vars[1].value.bytes.as_deref(), Ok(&[7, 0, 0, 0, 0, 0][..]));
    assert_eq!(vars[2].value.bytes.as_deref(), Ok(&[][..]));
    assert_eq!(vars[3].value.bytes, Err(Unavailable::SizeUnknown));
}
