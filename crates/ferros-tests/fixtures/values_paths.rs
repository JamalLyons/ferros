//! Values reached by `print <path>`: fields, tuple fields, indexes and dereferences.
#![expect(dead_code, reason = "the debugger reads these fields; the program does not")]

use std::cmp::Ordering;
use std::hint::black_box;

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

    black_box(&p); // BREAK: inspect
    black_box((&v, &boxed, &t, &named, &r, &rr, &arr, &slice, &raw, &null));
    black_box((&wrapper, &boxed_point, &grid, &empty, &count));
    black_box((&vr, &boxed_slice, &mut_slice));
    black_box((&shape, &sq, &empty_shape, &opt, &none, &res_ok, &res_err, &opt_ref, &ordering));
}
