//! Type-layer fixture program: types whose DWARF `tests/native/types.rs` resolves (#384).
//!
//! `main` prints, for every field of `Sample`, its offset in `Sample` (`offset_of!`), the
//! field type's name (`std::any::type_name`) and size (`size_of`), the field offsets of
//! `Point`, the tuple, `Node` and the enum payloads, and the declared discriminants of
//! `Shape`, one `key=value` per line. It then prints the address and size of three samples:
//! `None` and `Shape::Circle`, `Some(77)` and `Shape::Rect`, `None` and `Shape::Empty`. It prints
//! `ready`, and stops at marker `inspect`.
//!
//! Types:
//! - `Point` (struct; rustc reorders its fields)
//! - `(u8, Point)`, `[u16; 4]`, `&u16`, `*const u16`, `Option<u32>`
//! - `Shape` (`#[repr(u8)]` enum with fields and explicit discriminants)
//! - `Node` (recursive: `next: Option<Box<Node>>`)
//!
//! Functions:
//! - `main`
//!
//! Markers:
//! - `inspect`

use std::any::type_name;
use std::hint::black_box;
use std::mem::{offset_of, size_of, size_of_val};
use std::ptr::from_ref;

const CIRCLE: u8 = 3;
const RECT: u8 = 7;
const EMPTY: u8 = 9;

static WORD: u16 = 0x5a5a;

pub struct Point
{
    pub x: i32,
    pub y: i64,
    pub tag: u8,
}

#[repr(u8)]
pub enum Shape
{
    Circle
    {
        radius: f64,
    } = CIRCLE,
    Rect
    {
        width: u32,
        height: u32,
    } = RECT,
    Empty = EMPTY,
}

pub struct Node
{
    pub value: u32,
    pub next: Option<Box<Node>>,
}

pub struct Sample
{
    pub point: Point,
    pub pair: (u8, Point),
    pub words: [u16; 4],
    pub word_ref: &'static u16,
    pub word_ptr: *const u16,
    pub maybe: Option<u32>,
    pub shape: Shape,
    pub list: Node,
}

/// Print `key.name`, `key.size` and `key.offset` for field `key` of `Sample`, of type `T`.
fn field<T>(key: &str, offset: usize)
{
    println!("{key}.name={}", type_name::<T>());
    println!("{key}.size={}", size_of::<T>());
    println!("{key}.offset={offset}");
}

fn sample(maybe: Option<u32>, shape: Shape, value: u32) -> Sample
{
    Sample {
        point: Point {
            x: -5,
            y: 1 << 40,
            tag: 0xab,
        },
        pair: (0x11, Point { x: 6, y: 7, tag: 8 }),
        words: [1, 2, 3, 4],
        word_ref: &WORD,
        word_ptr: &raw const WORD,
        maybe,
        shape,
        list: Node {
            value,
            next: Some(Box::new(Node {
                value: value + 1,
                next: None,
            })),
        },
    }
}

fn payload_layout(sample: &Sample)
{
    if let Some(value) = &sample.maybe {
        let base = from_ref(&sample.maybe).addr();
        println!("maybe.Some.field.0={}", from_ref(value).addr() - base);
    }
    let base = from_ref(&sample.shape).addr();
    match &sample.shape {
        Shape::Circle { radius } => println!("shape.Circle.field.radius={}", from_ref(radius).addr() - base),
        Shape::Rect { width, height } => {
            println!("shape.Rect.field.width={}", from_ref(width).addr() - base);
            println!("shape.Rect.field.height={}", from_ref(height).addr() - base);
        }
        Shape::Empty => {}
    }
}

fn main()
{
    println!("sample.name={}", type_name::<Sample>());
    println!("sample.size={}", size_of::<Sample>());
    field::<Point>("point", offset_of!(Sample, point));
    field::<(u8, Point)>("pair", offset_of!(Sample, pair));
    field::<[u16; 4]>("words", offset_of!(Sample, words));
    field::<&u16>("word_ref", offset_of!(Sample, word_ref));
    field::<*const u16>("word_ptr", offset_of!(Sample, word_ptr));
    field::<Option<u32>>("maybe", offset_of!(Sample, maybe));
    field::<Shape>("shape", offset_of!(Sample, shape));
    field::<Node>("list", offset_of!(Sample, list));
    println!("point.field.x={}", offset_of!(Point, x));
    println!("point.field.y={}", offset_of!(Point, y));
    println!("point.field.tag={}", offset_of!(Point, tag));
    println!("pair.field.0={}", offset_of!((u8, Point), 0));
    println!("pair.field.1={}", offset_of!((u8, Point), 1));
    println!("list.field.value={}", offset_of!(Node, value));
    println!("list.field.next={}", offset_of!(Node, next));
    println!("shape.discriminant.Circle={CIRCLE}");
    println!("shape.discriminant.Rect={RECT}");
    println!("shape.discriminant.Empty={EMPTY}");

    #[expect(
        clippy::large_stack_arrays,
        reason = "Expose a stack-local array in the fixture's debug info"
    )]
    let samples = [
        sample(None, Shape::Circle { radius: 2.5 }, 10),
        sample(Some(77), Shape::Rect { width: 640, height: 480 }, 20),
        sample(None, Shape::Empty, 30),
    ];
    for sample in &samples {
        payload_layout(sample);
    }
    println!("samples.address={:p}", &raw const samples);
    println!("samples.size={}", size_of_val(&samples));
    println!("ready");
    black_box(&samples); // BREAK: inspect
}
