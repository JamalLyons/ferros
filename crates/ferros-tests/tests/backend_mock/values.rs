//! Public variable inspection over synthetic DWARF and a stopped mock process.

use std::io::Write as _;

use ferros_core::backend::mock::{MockBackend, MockCall, arm64_registers};
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
    let (session, _, cleanup) = session_with(dwarf, omit, |backend| backend);
    (session, cleanup)
}

/// [`session`], with `extra` adding regions and images to the backend, which is returned too.
fn session_with(
    dwarf: &mut DwarfUnit,
    omit: Option<SectionId>,
    extra: impl FnOnce(MockBackend) -> MockBackend,
) -> (Session, MockBackend, RemoveOnDrop)
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
    let backend = extra(backend);
    let mut session = Session::new(Box::new(backend.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("mock launch");
    (session, backend, cleanup)
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
fn trait_objects_distinguish_exact_inferred_and_unknown_vtable_types()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let root = unit.root();
    unit.get_mut(byte)
        .set(gimli::DW_AT_encoding, Attr::Encoding(gimli::DW_ATE_unsigned));
    let concrete = unit.add(root, gimli::DW_TAG_structure_type);
    unit.get_mut(concrete)
        .set(gimli::DW_AT_name, Attr::String(b"Reading".to_vec()));
    unit.get_mut(concrete).set(gimli::DW_AT_byte_size, Attr::Udata(1));
    let count = variable(unit, concrete, gimli::DW_TAG_member, Attr::String(b"count".to_vec()), byte);
    unit.get_mut(count).set(gimli::DW_AT_data_member_location, Attr::Udata(0));

    let storage = Address::new(BASE + 0x400);
    let data = Address::new(BASE + 0x450);
    let table = Address::new(BASE + 0x500);
    let vtable_type = unit.add(root, gimli::DW_TAG_structure_type);
    unit.get_mut(vtable_type)
        .set(gimli::DW_AT_containing_type, Attr::UnitRef(concrete));
    let vtable = variable(
        unit,
        root,
        gimli::DW_TAG_variable,
        Attr::String(b"<Reading as Shape>::{vtable}".to_vec()),
        vtable_type,
    );
    let mut location = Expression::new();
    location.op_addr(DwarfAddress::Constant(table.value()));
    unit.get_mut(vtable).set(gimli::DW_AT_location, Attr::Exprloc(location));

    let method = Address::new(BASE + 0x200);
    let implementation = unit.add(root, gimli::DW_TAG_subprogram);
    let implementation = unit.get_mut(implementation);
    implementation.set(gimli::DW_AT_name, Attr::String(b"<Reading as Shape>::area".to_vec()));
    implementation.set(gimli::DW_AT_low_pc, Attr::Address(DwarfAddress::Constant(method.value())));
    implementation.set(gimli::DW_AT_high_pc, Attr::Udata(8));

    let erased = unit.add(root, gimli::DW_TAG_structure_type);
    unit.get_mut(erased)
        .set(gimli::DW_AT_name, Attr::String(b"dyn Shape".to_vec()));
    unit.get_mut(erased).set(gimli::DW_AT_byte_size, Attr::Udata(0));
    let word = index_type(unit, gimli::DW_ATE_unsigned, 8);
    let entries = array_with_bounds(unit, word, None, Attr::Udata(0), Attr::Udata(3));
    let trait_object = unit.add(root, gimli::DW_TAG_structure_type);
    unit.get_mut(trait_object)
        .set(gimli::DW_AT_name, Attr::String(b"&dyn Shape".to_vec()));
    unit.get_mut(trait_object).set(gimli::DW_AT_byte_size, Attr::Udata(16));
    let mut add_member = |name: &[u8], offset, pointee| {
        let pointer = unit.add(root, gimli::DW_TAG_pointer_type);
        unit.get_mut(pointer).set(gimli::DW_AT_type, Attr::UnitRef(pointee));
        let field = variable(unit, trait_object, gimli::DW_TAG_member, Attr::String(name.to_vec()), pointer);
        unit.get_mut(field)
            .set(gimli::DW_AT_data_member_location, Attr::Udata(offset));
    };
    add_member(b"pointer", 0, erased);
    add_member(b"vtable", 8, entries);
    let mut location = Expression::new();
    location.op_addr(DwarfAddress::Constant(storage.value()));
    local(unit, function, "object", trait_object, location);

    let (mut session, _image) = session(&mut dwarf, None);
    session.write_memory(data, &[42]).expect("store object data");
    let inferred = table + 0x20;
    let unknown = table + 0x40;
    for (vtable, entry) in [(inferred, method), (unknown, method + 4)] {
        let bytes = [0, 1, 1, entry.value()]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect::<Vec<_>>();
        session.write_memory(vtable, &bytes).expect("store mock vtable");
    }
    let addresses = format!("dyn {{ data: {data}, vtable: {unknown} }}");
    let mut assert_rendered = |vtable: Address, expected: &str| {
        let bytes = [data.value().to_le_bytes(), vtable.value().to_le_bytes()].concat();
        session.write_memory(storage, &bytes).expect("store fat pointer");
        let vars = session.selected_frame_variables().expect("read locals");
        assert_eq!(session.format_variable(&vars[0], FormatOptions::default()).1, expected);
    };
    assert_rendered(table, "Reading { count: 42 }");
    assert_rendered(inferred, "Reading { count: 42 } (type inferred from vtable methods)");
    assert_rendered(unknown, &addresses);
}

#[test]
fn function_pointers_without_dwarf_name_only_an_exact_symbol_table_entry()
{
    // Issue #419: a function pointer into code with only a symbol table is named from it,
    // but only at the function's first instruction. The library image has no DWARF and a
    // table naming `c_entry` and `c_other`; a second listed image has no readable header.
    const LIBRARY: u64 = 0x2_0000_0000;
    const UNREADABLE: u64 = 0x5_0000_0000;
    const OUTSIDE: u64 = 0x9_0000_0000;
    const SIGNATURE: u64 = 0xa532_0000_0000_0000;
    const POINTERS: &[(&str, u64)] = &[
        ("entry", LIBRARY + 0x1000),
        ("inside", LIBRARY + 0x1004),
        ("signed_entry", SIGNATURE | (LIBRARY + 0x1000)),
        ("signed_inside", SIGNATURE | (LIBRARY + 0x1004)),
        ("signed_with_dwarf", SIGNATURE | (BASE + 0x100)),
        ("signed_inside_dwarf", SIGNATURE | (BASE + 0x104)),
        ("other", LIBRARY + 0x1040),
        ("outside", OUTSIDE),
        ("outside_too", OUTSIDE + 0x40),
        ("with_dwarf", BASE + 0x100),
    ];
    let (mut dwarf, function, _) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let signature = unit.add(unit.root(), gimli::DW_TAG_subroutine_type);
    let pointer = pointer_to(unit, "fn()", signature);
    for (index, &(name, _)) in (0_u64..).zip(POINTERS) {
        static_local(unit, function, name, pointer, Address::new(BASE + 0x300 + 8 * index));
    }
    let pair = named_struct(unit, unit.root(), "Pair", 16);
    add_member(unit, pair, "first", pointer, 0);
    add_member(unit, pair, "second", pointer, 8);
    static_local(unit, function, "pair", pair, Address::new(BASE + 0x300));

    let library = MockObjectImage::new(LIBRARY + 0x1000..LIBRARY + 0x1100, LIBRARY + 0x2000, LIBRARY + 0x3000)
        .with_symbol(LIBRARY + 0x1000, "c_entry")
        .with_symbol(LIBRARY + 0x1040, "c_other");
    let mut bytes = library.header_bytes();
    bytes.resize(0x2000, 0x1F);
    bytes.extend(library.table_bytes());
    bytes.resize(0x3000, 0);
    bytes.extend(library.name_bytes());
    let listed = |path: &str, address| LoadedImage {
        path: format!("/nonexistent/{path}").into(),
        load_address: Address::new(address),
        build_id: BuildIdentity::Unknown,
        role: ImageRole::Library,
    };
    let (mut session, mock, _image) = session_with(&mut dwarf, None, |backend| {
        backend
            .with_region(Address::new(LIBRARY), "r-x", bytes)
            .with_image(listed("ferros-mock-c-library", LIBRARY))
            .with_image(listed("ferros-mock-unreadable", UNREADABLE))
    });
    let values = words(&POINTERS.iter().map(|&(_, address)| address).collect::<Vec<_>>());
    session
        .write_memory(Address::new(BASE + 0x300), &values)
        .expect("store pointers");
    let vars = session.selected_frame_variables().expect("read locals");
    let mut images = session.images().expect("listed images");
    images.retain(|image| image.load_address != Address::new(LIBRARY));
    let mut print = |name: &str| {
        let var = vars.iter().find(|var| var.name == name).expect(name);
        session.format_variable(var, FormatOptions::default()).1
    };
    assert_eq!(print("entry"), format!("{:#x} <c_entry>", LIBRARY + 0x1000));
    assert_eq!(print("inside"), format!("{:#x}", LIBRARY + 0x1004), "not an entry");
    assert_eq!(print("other"), format!("{:#x} <c_other>", LIBRARY + 0x1040));
    assert_eq!(print("with_dwarf"), format!("{:#x} <function>", BASE + 0x100));
    assert_eq!(print("outside"), format!("{OUTSIDE:#x}"));
    assert_eq!(
        print("signed_entry"),
        format!("{:#x} <c_entry>", SIGNATURE | (LIBRARY + 0x1000))
    );
    assert_eq!(print("signed_inside"), format!("{:#x}", SIGNATURE | (LIBRARY + 0x1004)));
    assert_eq!(
        print("signed_with_dwarf"),
        format!("{:#x} <function>", SIGNATURE | (BASE + 0x100))
    );
    assert_eq!(print("signed_inside_dwarf"), format!("{:#x}", SIGNATURE | (BASE + 0x104)));

    // Printing again reuses the same symbol data, including misses for unreadable
    // headers and addresses outside every image.
    let _ = mock.take_calls();
    assert_eq!(print("outside"), format!("{OUTSIDE:#x}"));
    assert_eq!(print("outside_too"), format!("{:#x}", OUTSIDE + 0x40));
    assert_eq!(print("inside"), format!("{:#x}", LIBRARY + 0x1004));
    assert_eq!(print("entry"), format!("{:#x} <c_entry>", LIBRARY + 0x1000));
    let repeated_calls = mock.take_calls();

    // Replace the loader snapshot without another stop or a stack trace.
    mock.set_images(images);
    assert_eq!(
        print("pair"),
        format!("Pair {{ first: {:#x}, second: {:#x} }}", LIBRARY + 0x1000, LIBRARY + 0x1004),
        "the library was unloaded"
    );
    assert_eq!(
        repeated_calls,
        vec![MockCall::Images; 4],
        "each render refreshes once and reuses symbol data"
    );
    assert_eq!(mock.take_calls(), vec![MockCall::Images], "the whole pair shares one refresh");
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
    const NAMES: &[&str] = &[
        "&str",
        "alloc::vec::Vec<u8, alloc::alloc::Global>",
        "std::collections::hash::map::HashMap<u8, u8>",
        "std::collections::hash::set::HashSet<u8>",
        "alloc::collections::btree::map::BTreeMap<u8, u8>",
        "alloc::collections::btree::set::BTreeSet<u8>",
        "alloc::collections::vec_deque::VecDeque<u8>",
        "alloc::collections::linked_list::LinkedList<u8>",
        "alloc::collections::binary_heap::BinaryHeap<u8>",
        "std::sync::poison::mutex::Mutex<u8>",
        "std::sync::poison::rwlock::RwLock<u8>",
        "std::sync::poison::mutex::MutexGuard<u8>",
        "std::sync::poison::rwlock::RwLockReadGuard<u8>",
        "std::sync::poison::rwlock::RwLockWriteGuard<u8>",
        "core::sync::atomic::Atomic<u8>",
        "core::sync::atomic::Atomic<bool>",
        "core::cell::once::OnceCell<u8>",
        "std::sync::once_lock::OnceLock<u8>",
        "core::cell::UnsafeCell<u8>",
        "core::pin::Pin<&u8>",
        "alloc::rc::Weak<u8, alloc::alloc::Global>",
        "alloc::sync::Weak<u8, alloc::alloc::Global>",
    ];
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

#[test]
fn lock_and_guard_reject_incompatible_data_field()
{
    const LOCK: &str = "std::sync::poison::mutex::Mutex<u8>";
    const GUARD: &str = "std::sync::poison::mutex::MutexGuard<u8>";
    const CASES: &[(&str, &str)] = &[("guard", GUARD), ("lock", LOCK)];
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let root = unit.root();
    let pointer = pointer_to(unit, "*mut u8", byte);
    let lock = named_struct(unit, root, LOCK, 16);
    add_member(unit, lock, "inner", byte, 0);
    add_member(unit, lock, "data", pointer, 8);
    let reference = pointer_to(unit, "&Mutex<u8>", lock);
    let guard = named_struct(unit, root, GUARD, 8);
    add_member(unit, guard, "lock", reference, 0);
    let lock_address = Address::new(BASE + 0x200);
    let guard_address = Address::new(BASE + 0x220);
    static_local(unit, function, "lock", lock, lock_address);
    static_local(unit, function, "guard", guard, guard_address);

    let (mut session, _image) = session(&mut dwarf, None);
    session.write_memory(lock_address, &words(&[0, 0])).expect("store mock lock");
    session
        .write_memory(guard_address, &words(&[lock_address.value()]))
        .expect("store mock guard");
    let vars = session.selected_frame_variables().expect("mock values");
    for &(name, ty) in CASES {
        let var = vars.iter().find(|var| var.name == name).expect(name);
        let text = session.format_variable(var, FormatOptions::default()).1;
        assert!(text.starts_with(ty) && text.ends_with(" <unrecognized layout>"), "{text}");
    }
}

fn named_struct(unit: &mut Unit, parent: UnitEntryId, name: &str, size: u64) -> UnitEntryId
{
    let ty = unit.add(parent, gimli::DW_TAG_structure_type);
    unit.get_mut(ty)
        .set(gimli::DW_AT_name, Attr::String(name.as_bytes().to_vec()));
    unit.get_mut(ty).set(gimli::DW_AT_byte_size, Attr::Udata(size));
    ty
}

fn add_member(unit: &mut Unit, parent: UnitEntryId, name: &str, ty: UnitEntryId, offset: u64)
{
    let member = variable(unit, parent, gimli::DW_TAG_member, Attr::String(name.as_bytes().to_vec()), ty);
    unit.get_mut(member)
        .set(gimli::DW_AT_data_member_location, Attr::Udata(offset));
}

fn element_param(unit: &mut Unit, parent: UnitEntryId, ty: UnitEntryId)
{
    variable(
        unit,
        parent,
        gimli::DW_TAG_template_type_parameter,
        Attr::String(b"T".to_vec()),
        ty,
    );
}

fn pointer_to(unit: &mut Unit, name: &str, pointee: UnitEntryId) -> UnitEntryId
{
    let ty = variable(
        unit,
        unit.root(),
        gimli::DW_TAG_pointer_type,
        Attr::String(name.as_bytes().to_vec()),
        pointee,
    );
    unit.get_mut(ty).set(gimli::DW_AT_byte_size, Attr::Udata(8));
    ty
}

fn static_local(unit: &mut Unit, function: UnitEntryId, name: &str, ty: UnitEntryId, address: Address)
{
    let mut location = Expression::new();
    location.op_addr(DwarfAddress::Constant(address.value()));
    local(unit, function, name, ty, location);
}

fn words(values: &[u64]) -> Vec<u8>
{
    values.iter().copied().flat_map(u64::to_le_bytes).collect()
}

/// `u8`, `usize` and a `RawVec<u8>` of `{ ptr, cap }` for the collection tests.
fn raw_vec_types(unit: &mut Unit, byte: UnitEntryId) -> (UnitEntryId, UnitEntryId)
{
    unit.get_mut(byte)
        .set(gimli::DW_AT_encoding, Attr::Encoding(gimli::DW_ATE_unsigned));
    let usize = index_type(unit, gimli::DW_ATE_unsigned, 8);
    unit.get_mut(usize).set(gimli::DW_AT_name, Attr::String(b"usize".to_vec()));
    let data = pointer_to(unit, "*const u8", byte);
    let raw_vec = named_struct(unit, unit.root(), "alloc::raw_vec::RawVec<u8>", 16);
    add_member(unit, raw_vec, "ptr", data, 0);
    add_member(unit, raw_vec, "cap", usize, 8);
    (usize, raw_vec)
}

#[test]
fn vec_deque_and_binary_heap_reject_corrupt_headers()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let (usize, raw_vec) = raw_vec_types(unit, byte);
    let deque = named_struct(unit, unit.root(), "alloc::collections::vec_deque::VecDeque<u8>", 32);
    element_param(unit, deque, byte);
    add_member(unit, deque, "head", usize, 0);
    add_member(unit, deque, "len", usize, 8);
    add_member(unit, deque, "buf", raw_vec, 16);
    let vec = named_struct(unit, unit.root(), "alloc::vec::Vec<u8>", 24);
    element_param(unit, vec, byte);
    add_member(unit, vec, "buf", raw_vec, 0);
    add_member(unit, vec, "len", usize, 16);
    let heap = named_struct(unit, unit.root(), "alloc::collections::binary_heap::BinaryHeap<u8>", 24);
    element_param(unit, heap, byte);
    add_member(unit, heap, "data", vec, 0);
    let deque_at = Address::new(BASE + 0x200);
    let heap_at = Address::new(BASE + 0x240);
    let buffer = Address::new(BASE + 0x300);
    static_local(unit, function, "deque", deque, deque_at);
    static_local(unit, function, "heap", heap, heap_at);

    let (mut session, _image) = session(&mut dwarf, None);
    session.write_memory(buffer, &[1, 2, 3, 4]).expect("store elements");
    let mut render = |name: &str, at: Address, header: &[u64]| {
        session.write_memory(at, &words(header)).expect("store header");
        let vars = session.selected_frame_variables().expect("collection locals");
        let var = vars.iter().find(|var| var.name == name).expect(name);
        session.format_variable(var, FormatOptions::default()).1
    };
    let ptr = buffer.value();
    // VecDeque: head, len, ptr, cap.
    assert_eq!(render("deque", deque_at, &[3, 2, ptr, 4]), "[4, 1] (len: 2, capacity: 4)");
    assert_eq!(
        render("deque", deque_at, &[4, 1, ptr, 4]),
        "<unavailable: VecDeque head 4 is outside capacity 4>"
    );
    assert_eq!(
        render("deque", deque_at, &[0, 5, ptr, 4]),
        "<unavailable: VecDeque length 5 exceeds capacity 4>"
    );
    assert_eq!(
        render("deque", deque_at, &[0, 1, 0, 4]),
        "<unavailable: null VecDeque buffer pointer>"
    );
    // BinaryHeap: ptr, cap, len.
    assert_eq!(render("heap", heap_at, &[ptr, 4, 3]), "[1, 2, 3] (len: 3, capacity: 4)");
    assert_eq!(
        render("heap", heap_at, &[ptr, 4, 5]),
        "<unavailable: heap length 5 exceeds capacity 4>"
    );
}

#[test]
fn linked_list_stops_at_a_repeated_or_missing_node()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let (usize, _) = raw_vec_types(unit, byte);
    let root = unit.root();
    // LinkedList<u8> { head: Option<NonNull<Node<u8>>> @0, len @8 }, Node<u8> { next @0, element @8 }.
    let node = named_struct(unit, root, "alloc::collections::linked_list::Node<u8>", 16);
    let node_pointer = pointer_to(unit, "*const alloc::collections::linked_list::Node<u8>", node);
    let non_null = named_struct(unit, root, "core::ptr::non_null::NonNull<Node<u8>>", 8);
    add_member(unit, non_null, "pointer", node_pointer, 0);
    let option = named_struct(unit, root, "core::option::Option<NonNull<Node<u8>>>", 8);
    let some = named_struct(unit, option, "Some", 8);
    add_member(unit, some, "__0", non_null, 0);
    let variants = unit.add(option, gimli::DW_TAG_variant_part);
    let variant = unit.add(variants, gimli::DW_TAG_variant);
    add_member(unit, variant, "Some", some, 0);
    add_member(unit, node, "next", option, 0);
    add_member(unit, node, "element", byte, 8);
    let list = named_struct(unit, root, "alloc::collections::linked_list::LinkedList<u8>", 16);
    element_param(unit, list, byte);
    add_member(unit, list, "head", option, 0);
    add_member(unit, list, "len", usize, 8);
    let list_at = Address::new(BASE + 0x200);
    static_local(unit, function, "list", list, list_at);

    let (mut session, _image) = session(&mut dwarf, None);
    let (first, second) = (Address::new(BASE + 0x300), Address::new(BASE + 0x310));
    let mut render = |len: u64, after_second: Address| {
        session
            .write_memory(first, &words(&[second.value(), 10]))
            .expect("store first node");
        session
            .write_memory(second, &words(&[after_second.value(), 20]))
            .expect("store second node");
        session
            .write_memory(list_at, &words(&[first.value(), len]))
            .expect("store list header");
        let vars = session.selected_frame_variables().expect("list local");
        session.format_variable(&vars[0], FormatOptions::default()).1
    };
    assert_eq!(render(2, Address::ZERO), "[10, 20]");
    assert_eq!(render(3, first), "[10, 20, <unavailable: repeated LinkedList node>] (len: 3)");
    assert_eq!(
        render(3, Address::ZERO),
        "[10, 20, <unavailable: LinkedList has fewer nodes than its length>] (len: 3)"
    );
}

#[test]
fn vec_into_iter_rejects_a_reversed_range_and_truncates_at_max_elements()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    // Marks `u8` unsigned.
    raw_vec_types(unit, byte);
    let data = pointer_to(unit, "*const u8", byte);
    // IntoIter<u8> { ptr @0, end @8 }; the allocation fields are not read.
    let iter = named_struct(
        unit,
        unit.root(),
        "alloc::vec::into_iter::IntoIter<u8, alloc::alloc::Global>",
        16,
    );
    element_param(unit, iter, byte);
    add_member(unit, iter, "ptr", data, 0);
    add_member(unit, iter, "end", data, 8);
    let iter_at = Address::new(BASE + 0x200);
    static_local(unit, function, "iter", iter, iter_at);

    let (mut session, _image) = session(&mut dwarf, None);
    let buffer = Address::new(BASE + 0x300);
    session.write_memory(buffer, &[1, 2, 3, 4]).expect("store elements");
    let mut render = |ptr: u64, end: u64, options: FormatOptions| {
        session.write_memory(iter_at, &words(&[ptr, end])).expect("store iterator");
        let vars = session.selected_frame_variables().expect("iterator local");
        session.format_variable(&vars[0], options).1
    };
    let start = buffer.value();
    assert_eq!(render(start + 1, start + 4, FormatOptions::default()), "[2, 3, 4]");
    let limited = FormatOptions {
        max_elements: 2,
        ..FormatOptions::default()
    };
    assert_eq!(render(start, start + 4, limited), "[1, 2, ...]");
    // `end` before `ptr` is no valid range, so the raw fields show.
    let reversed = render(start + 2, start, FormatOptions::default());
    assert!(reversed.ends_with(" <unrecognized layout>"), "{reversed}");
}

#[test]
fn print_path_through_an_rc_reports_an_unreadable_payload_instead_of_the_rcs_own_field()
{
    let (mut dwarf, function, byte) = dwarf_function(Some("function"));
    let unit = &mut dwarf.unit;
    let (usize, _) = raw_vec_types(unit, byte);
    let root = unit.root();
    // Rc<Payload> { ptr: NonNull { pointer } }, RcInner { strong, weak, value: Payload { ptr: u8 } }.
    let unsafe_cell = named_struct(unit, root, "core::cell::UnsafeCell<usize>", 8);
    add_member(unit, unsafe_cell, "value", usize, 0);
    let cell = named_struct(unit, root, "core::cell::Cell<usize>", 8);
    add_member(unit, cell, "value", unsafe_cell, 0);
    let payload = named_struct(unit, root, "Payload", 1);
    add_member(unit, payload, "ptr", byte, 0);
    let inner = named_struct(unit, root, "alloc::rc::RcInner<Payload>", 24);
    add_member(unit, inner, "strong", cell, 0);
    add_member(unit, inner, "weak", cell, 8);
    add_member(unit, inner, "value", payload, 16);
    let inner_pointer = pointer_to(unit, "*const alloc::rc::RcInner<Payload>", inner);
    let non_null = named_struct(unit, root, "core::ptr::non_null::NonNull<alloc::rc::RcInner<Payload>>", 8);
    add_member(unit, non_null, "pointer", inner_pointer, 0);
    let rc = named_struct(unit, root, "alloc::rc::Rc<Payload, alloc::alloc::Global>", 8);
    add_member(unit, rc, "ptr", non_null, 0);
    let rc_at = Address::new(BASE + 0x200);
    static_local(unit, function, "rc", rc, rc_at);

    let (mut session, _image) = session(&mut dwarf, None);
    let allocation = Address::new(BASE + 0x300);
    let mut allocation_bytes = words(&[1, 1]);
    allocation_bytes.push(42);
    session.write_memory(allocation, &allocation_bytes).expect("store allocation");
    let path = ferros_core::values::path::Path::parse("rc.ptr").expect("parse rc.ptr");
    let mut print = |target: u64| {
        session.write_memory(rc_at, &words(&[target])).expect("store Rc");
        let vars = session.selected_frame_variables().expect("rc local");
        session
            .format_path(&vars[0], &path, FormatOptions::default())
            .map(|(_, text)| text)
            .map_err(|error| error.to_string())
    };
    // The payload's `ptr` wins over the Rc's own `ptr`, as Rust's auto-dereference does.
    assert_eq!(print(allocation.value()), Ok("42".to_owned()));
    // An unreadable allocation is the error, not a silent fall back to the Rc's NonNull.
    let unmapped = BASE + IMAGE_SIZE;
    assert_eq!(
        print(unmapped),
        Err(format!("`rc`: not found: address {unmapped:#x} is not mapped"))
    );
}
