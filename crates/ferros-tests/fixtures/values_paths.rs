//! Values reached by `print <path>`: fields, tuple fields, indexes and dereferences,
//! through smart pointers, cells and containers.
#![expect(dead_code, reason = "the debugger reads these fields; the program does not")]

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::hint::black_box;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;

struct Inner
{
    x: i32,
    y: i32,
}

struct Point
{
    inner: Inner,
    label: &'static str,
}

struct Named
{
    name: String,
    id: u16,
}

struct Wrapper(u8, Inner);

enum Shape
{
    Circle
    {
        r: f64,
    },
    Square(u32, Inner),
    Empty,
}

enum Single
{
    Value(u16),
}

/// Over the debugger's 1 MiB value cap, so only the active variant can be read.
#[expect(clippy::large_enum_variant, reason = "the size is what the test needs")]
enum Huge
{
    Small(u8),
    Large([u8; 2 << 20]),
}

fn main()
{
    let p = Point {
        inner: Inner { x: 5, y: -9 },
        label: "origin",
    };
    let v = vec![10_u32, 20, 30, 40];
    let boxed = Box::new(77_i64);
    let t = ('q', 314_u16, true);
    let named = Named {
        name: String::from("ferris"),
        id: 3,
    };
    let r = &named;
    let rr = &r;
    let arr = [1_i8, -2, 3];
    let slice: &[u32] = &v[1..];
    let raw = &raw const p.inner;
    let null: *const Inner = std::ptr::null();
    let wrapper = Wrapper(4, Inner { x: 8, y: 16 });
    let boxed_point = Box::new(Inner { x: -1, y: 2 });
    let grid = [[1_u8, 2], [3, 4]];
    let empty: Vec<u32> = Vec::new();
    let count = 6_u32;
    let vr = &v;
    let boxed_slice: Box<[u16]> = Box::new([11, 12, 13]);
    let mut storage = [5_i16, -6, 7];
    let mut_slice: &mut [i16] = &mut storage;
    let shape = Shape::Circle { r: 2.5 };
    let sq = Shape::Square(7, Inner { x: 70, y: 71 });
    let empty_shape = Shape::Empty;
    let opt = Some(42_u32);
    let none: Option<u32> = None;
    let res_ok: Result<u16, i32> = Ok(61);
    let res_err: Result<u16, i32> = Err(-71);
    let opt_ref = Some(&named);
    let ordering = Ordering::Less;
    let maybe_order = Some(Ordering::Greater);
    let opt_ptr = &opt;
    let single = Single::Value(51);
    let huge: Box<Huge> = Box::new(Huge::Small(5));
    let rc = Rc::new(Named {
        name: String::from("shared"),
        id: 9,
    });
    let arc: Arc<[u32]> = Arc::from([100_u32, 200, 300]);
    let pinned: Pin<Box<i64>> = Box::pin(-88);
    let cell = Cell::new(Inner { x: 21, y: 22 });
    let cell_struct = RefCell::new(Inner { x: 31, y: 32 });
    let weak = Rc::downgrade(&rc);
    let dangling: Weak<Named> = {
        let gone = Rc::new(Named {
            name: String::new(),
            id: 0,
        });
        Rc::downgrade(&gone)
    };
    let empty_weak: Weak<Named> = Weak::new();
    // Capacity 4 with the head moved to slot 2: logical [3, 4, 5, 6] is stored [5, 6, 3, 4].
    let mut vd: VecDeque<u32> = VecDeque::with_capacity(4);
    vd.extend([1, 2, 3]);
    vd.drain(..2);
    vd.extend([4, 5, 6]);
    let raw_slice: *const [u16] = storage_u16();
    let text = String::from("hey");
    let cow_borrowed: Cow<'_, str> = Cow::Borrowed("cow");
    let cow_owned: Cow<'_, [u8]> = Cow::Owned(vec![7, 8]);

    black_box(&p); // BREAK: inspect
    black_box((&v, &boxed, &t, &named, &r, &rr, &arr, &slice, &raw, &null));
    black_box((&wrapper, &boxed_point, &grid, &empty, &count));
    black_box((&vr, &boxed_slice, &mut_slice));
    black_box((&shape, &sq, &empty_shape, &opt, &none, &res_ok, &res_err, &opt_ref, &ordering));
    black_box((&maybe_order, &opt_ptr, &single, &huge));
    black_box((&rc, &arc, &pinned, &cell, &cell_struct, &weak, &dangling, &vd, &raw_slice));
    black_box(&empty_weak);
    black_box((&text, &cow_borrowed, &cow_owned));
}

/// A slice that outlives `main`'s raw pointer to it.
fn storage_u16() -> &'static [u16]
{
    &[10, 20, 30]
}
