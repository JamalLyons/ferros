//! The DWARF type layer (`ferros_core::values::types`) against a real fixture (issue #384).
//!
//! The `values_types` fixture prints the layout rustc gave its types (`offset_of!`,
//! `size_of`, `type_name`) and stops at marker `inspect`. The test resolves the same types
//! from the fixture's debug info and checks names, sizes, member offsets, array lengths,
//! pointees and enum discriminants against what the program printed, then reads the live
//! discriminants of three samples from memory and checks that the model's variant values
//! select the variant the program built.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::time::Duration;

use ferros_core::symbols::{BinaryImage, ImageDescriptor};
use ferros_core::values::types::{
    BaseEncoding, Composite, Discriminant, DiscriminantRange, PointerKind, RustEnum, Type, TypeId, TypeKind, TypeTable,
};
use ferros_core::{Address, BreakpointLocation, Session, StopEvent, SymbolCache};
use ferros_macos::MachOFormat;
use ferros_tests::{Fixture, fixture, harness};

/// How long the test waits for the stop at `inspect`.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The fixture's crate, which qualifies its type names.
const CRATE: &str = "ferros_fixture_values_types";

static SAMPLE_FIELDS: [&str; 8] = ["point", "pair", "words", "word_ref", "word_ptr", "maybe", "shape", "list"];
static EXPECTED_VARIANTS: [(&str, &str); 3] = [("None", "Circle"), ("Some", "Rect"), ("None", "Empty")];

/// The fixture's `key=value` lines.
fn layout(stdout: &str) -> HashMap<String, String>
{
    stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn number(layout: &HashMap<String, String>, key: &str) -> u64
{
    layout
        .get(key)
        .unwrap_or_else(|| panic!("the fixture printed no {key}"))
        .parse()
        .unwrap_or_else(|err| panic!("{key}: {err}"))
}

/// Launch the fixture, run it to `inspect`, and return the stopped session with what it
/// printed.
fn stop_at_inspect(t: &harness::TestContext, fixture: &Fixture) -> (harness::Debuggee, HashMap<String, String>)
{
    let mut d = t.launch(fixture);
    d.set_breakpoint(BreakpointLocation::Line {
        file: format!("{}.rs", fixture.name()),
        line: fixture.marker_line("inspect"),
    })
    .expect("set_breakpoint at inspect");
    d.resume().expect("resume");
    let stop = d.wait(Some(STOP_TIMEOUT)).expect("wait").expect("a stop");
    assert!(matches!(stop, StopEvent::Breakpoint { .. }), "got {stop:?}");
    assert!(d.wait_for_stdout("ready", STOP_TIMEOUT), "stdout: {}", d.stdout());
    let printed = layout(&d.stdout());
    (d, printed)
}

/// The fixture's main executable with its debug info, at its runtime load address.
fn main_image(session: &mut Session, fixture: &Fixture) -> std::sync::Arc<BinaryImage>
{
    let images = session.images().expect("images");
    let main = images
        .iter()
        .find(|image| image.path == fixture.path())
        .expect("the main executable image");
    SymbolCache::new(MachOFormat::shared())
        .load_image(ImageDescriptor {
            path: main.path.clone(),
            load_address: main.load_address.value(),
        })
        .expect("fixture image with its debug info")
}

fn get(types: &TypeTable, id: TypeId) -> std::sync::Arc<Type>
{
    types.get(id).expect("an id of this table")
}

fn check_unsigned_base(types: &TypeTable, id: TypeId, name: &str, size: u64)
{
    let ty = get(types, id);
    assert_eq!(ty.name.as_deref(), Some(name));
    assert_eq!(ty.kind, TypeKind::Base(BaseEncoding::Unsigned));
    assert_eq!(ty.size, Some(size));
}

fn composite(types: &TypeTable, id: TypeId) -> Composite
{
    match &get(types, id).kind {
        TypeKind::Struct(fields) | TypeKind::Tuple(fields) => fields.clone(),
        other => panic!("{:?} is not a struct: {other:?}", get(types, id).name),
    }
}

fn rust_enum(types: &TypeTable, id: TypeId) -> RustEnum
{
    match &get(types, id).kind {
        TypeKind::Enum(value) => value.clone(),
        other => panic!("{:?} is not an enum: {other:?}", get(types, id).name),
    }
}

/// The single discriminant value of `variant`.
fn value_of(value: &RustEnum, variant: &str) -> u128
{
    match &value
        .variant(variant)
        .unwrap_or_else(|| panic!("no variant {variant}"))
        .discriminant
    {
        Discriminant::Values(ranges) => match ranges.as_slice() {
            [DiscriminantRange { low, high }] if low == high => *low,
            other => panic!("{variant}: {other:?}"),
        },
        Discriminant::Default => panic!("{variant} has no value"),
    }
}

/// The raw discriminant of the enum `value` stored at `address`.
fn read_discriminant(session: &mut Session, types: &TypeTable, value: &RustEnum, address: u64) -> u128
{
    let discriminant = value.discriminant.as_ref().expect("a discriminant member");
    let size = get(types, discriminant.ty.expect("typed discriminant")).size.expect("sized");
    let at = address + discriminant.offset.expect("discriminant offset");
    let bytes = session
        .read_memory(Address::new(at), usize::try_from(size).expect("size"))
        .expect("read discriminant");
    bytes.iter().rev().fold(0, |raw, &byte| (raw << 8) | u128::from(byte))
}

fn check_sample_fields(types: &TypeTable, sample_fields: &Composite, printed: &HashMap<String, String>)
{
    // Every field: its offset, and its type's name and size, as the program has them.
    for &field in &SAMPLE_FIELDS {
        let member = sample_fields.member(field).unwrap_or_else(|| panic!("no field {field}"));
        assert_eq!(member.offset, Some(number(printed, &format!("{field}.offset"))), "{field}");
        let ty = get(types, member.ty.expect("typed member"));
        assert_eq!(
            ty.name.as_deref(),
            Some(printed[&format!("{field}.name")].as_str()),
            "{field}"
        );
        assert_eq!(ty.size, Some(number(printed, &format!("{field}.size"))), "{field}");
        assert_eq!(
            types.type_named(&printed[&format!("{field}.name")]),
            member.ty,
            "{field} by name"
        );
    }
    let field_type = |field: &str| sample_fields.member(field).and_then(|member| member.ty).expect("typed");

    // A struct whose fields rustc reordered, and a tuple.
    let point = composite(types, field_type("point"));
    for name in ["x", "y", "tag"] {
        let offset = point.member(name).and_then(|member| member.offset);
        assert_eq!(offset, Some(number(printed, &format!("point.field.{name}"))), "Point::{name}");
    }
    assert!(matches!(get(types, field_type("pair")).kind, TypeKind::Tuple(_)));
    let pair = composite(types, field_type("pair"));
    for index in ["0", "1"] {
        let offset = pair.member(&format!("__{index}")).and_then(|member| member.offset);
        assert_eq!(offset, Some(number(printed, &format!("pair.field.{index}"))), "pair.{index}");
    }

    // An array with its length, a reference and a raw pointer with their pointee.
    let TypeKind::Array(words) = &get(types, field_type("words")).kind else {
        panic!("[u16; 4] is an array");
    };
    check_unsigned_base(types, words.element.expect("array element type"), "u16", 2);
    assert_eq!(words.lengths, [Some(4)]);
    for (field, kind) in [
        ("word_ref", PointerKind::Reference { mutable: false }),
        ("word_ptr", PointerKind::Raw { mutable: false }),
    ] {
        let pointer = get(types, field_type(field));
        let TypeKind::Pointer(target) = &pointer.kind else {
            panic!("{field} is a pointer");
        };
        assert_eq!(target.kind, kind, "{field}");
        check_unsigned_base(types, target.pointee.expect("pointee type"), "u16", 2);
        assert_eq!(types.type_of(pointer.die), target.pointee, "{field}: DW_AT_type of the entry");
        assert_eq!(
            types.type_at(pointer.die),
            Some(field_type(field)),
            "{field}: same id by offset"
        );
    }
}

fn check_enum_layouts(types: &TypeTable, maybe: &RustEnum, shape: &RustEnum, printed: &HashMap<String, String>)
{
    // Option<u32>: a tagged enum, `Some`'s payload after the tag, `T` = u32.
    check_unsigned_base(types, types.type_named("u32").expect("u32 by name"), "u32", 4);
    assert_eq!((value_of(maybe, "None"), value_of(maybe, "Some")), (0, 1));
    let some = maybe.variant("Some").expect("Some");
    let payload = composite(types, some.members[0].ty.expect("Some payload"));
    let field = payload.member("__0").expect("Some field");
    assert_eq!(field.offset, Some(number(printed, "maybe.Some.field.0")));
    // A reference can point at a different CU's equivalent base-type DIE.
    for id in [
        field.ty,
        payload.template_param("T").and_then(|param| param.ty),
        maybe.template_param("T").and_then(|param| param.ty),
    ] {
        check_unsigned_base(types, id.expect("typed enum payload or parameter"), "u32", 4);
    }

    // A fieldful #[repr(u8)] enum: explicit discriminants and per-variant fields.
    assert_eq!(shape.variants.len(), 3);
    for variant in ["Circle", "Rect", "Empty"] {
        let declared = number(printed, &format!("shape.discriminant.{variant}"));
        assert_eq!(value_of(shape, variant), u128::from(declared), "Shape::{variant}");
    }
    let rect = shape.variant("Rect").expect("Rect");
    let rect_fields = composite(types, rect.members[0].ty.expect("Rect payload"));
    assert_eq!(
        rect_fields
            .members
            .iter()
            .map(|member| member.name.as_deref())
            .collect::<Vec<_>>(),
        [Some("width"), Some("height")]
    );
    let circle = shape.variant("Circle").expect("Circle");
    let circle_fields = composite(types, circle.members[0].ty.expect("Circle payload"));
    let check_offset = |variant: &str, fields: &Composite, name: &str| {
        let offset = fields.member(name).and_then(|member| member.offset);
        assert_eq!(offset, Some(number(printed, &format!("shape.{variant}.field.{name}"))));
    };
    check_offset("Rect", &rect_fields, "width");
    check_offset("Rect", &rect_fields, "height");
    check_offset("Circle", &circle_fields, "radius");
}

#[test]
fn types_resolve_with_the_layout_the_program_has()
{
    harness::run(|t| {
        let fixture = fixture!("values_types");
        let (mut d, printed) = stop_at_inspect(t, &fixture);
        let image = main_image(&mut d, &fixture);
        let types = image.types().expect("type table");
        assert!(std::ptr::eq(types, image.types().expect("cached type table")));

        let sample_name = format!("{CRATE}::Sample");
        assert_eq!(printed["sample.name"], sample_name);
        let sample = types.type_named(&sample_name).expect("Sample by qualified name");
        assert_eq!(get(types, sample).size, Some(number(&printed, "sample.size")));
        let sample_fields = composite(types, sample);

        check_sample_fields(types, &sample_fields, &printed);
        let field_type = |field: &str| sample_fields.member(field).and_then(|member| member.ty).expect("typed");
        let maybe = rust_enum(types, field_type("maybe"));
        let shape = rust_enum(types, field_type("shape"));
        check_enum_layouts(types, &maybe, &shape, &printed);

        // A recursive node: `next: Option<Box<Node>>` points back at the same id.
        let node = field_type("list");
        assert_eq!(types.type_named(&format!("{CRATE}::Node")), Some(node));
        let node_fields = composite(types, node);
        for name in ["value", "next"] {
            let offset = node_fields.member(name).and_then(|member| member.offset);
            assert_eq!(offset, Some(number(&printed, &format!("list.field.{name}"))), "Node::{name}");
        }
        let next = rust_enum(types, node_fields.member("next").and_then(|member| member.ty).expect("next"));
        assert_eq!(value_of(&next, "None"), 0);
        let boxed = next.variant("Some").expect("Some");
        assert_eq!(
            boxed.discriminant,
            Discriminant::Default,
            "a niche enum's dataful variant has no value"
        );
        let boxed = composite(types, boxed.members[0].ty.expect("Some payload"));
        let pointer = get(types, boxed.member("__0").and_then(|member| member.ty).expect("Box"));
        let TypeKind::Pointer(target) = &pointer.kind else {
            panic!("Box<Node> is a pointer");
        };
        assert_eq!((target.kind, target.pointee), (PointerKind::Box, Some(node)));

        // The live samples: each discriminant selects the variant the program built.
        let address = printed["samples.address"].trim_start_matches("0x");
        let base = u64::from_str_radix(address, 16).expect("hex samples.address");
        let stride = get(types, sample).size.expect("Sample size");
        assert_eq!(number(&printed, "samples.size"), 3 * stride);
        let offset = |field: &str| sample_fields.member(field).and_then(|member| member.offset).expect("offset");
        for (index, &(want_maybe, want_shape)) in (0_u64..).zip(EXPECTED_VARIANTS.iter()) {
            let at = base + index * stride;
            let mut check_variant = |field: &str, value: &RustEnum, want: &str| {
                let raw = read_discriminant(&mut d, types, value, at + offset(field));
                let found = value.variant_for(raw).and_then(|variant| variant.name.as_deref());
                assert_eq!(found, Some(want), "samples[{index}].{field} (raw {raw:#x})");
            };
            check_variant("maybe", &maybe, want_maybe);
            check_variant("shape", &shape, want_shape);
            let list = at + offset("list");
            let next_offset = node_fields
                .member("next")
                .and_then(|member| member.offset)
                .expect("next offset");
            let raw = read_discriminant(&mut d, types, &next, list + next_offset);
            let found = next.variant_for(raw).and_then(|variant| variant.name.as_deref());
            assert_eq!(found, Some("Some"), "samples[{index}].list.next (raw {raw:#x})");
        }
    });
}
