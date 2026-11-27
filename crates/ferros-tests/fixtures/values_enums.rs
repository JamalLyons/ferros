//! Active enum variants for the value formatter integration tests.

use std::hint::black_box;
use std::num::NonZeroU32;

#[repr(i16)]
enum Code
{
    Error = -7,
    Ready = 42,
}

#[repr(u8)]
pub enum Shape
{
    Point = 3,
    Circle(u16) = 7,
    Rect
    {
        width: u32,
        height: u16,
    } = 9,
}

pub enum Single
{
    Value(u16),
}

#[repr(C)]
pub struct Niche
{
    pub prefix: u32,
    pub value: NonZeroU32,
}

fn main()
{
    let code_error = Code::Error;
    let code_ready = Code::Ready;
    let point = Shape::Point;
    let circle = Shape::Circle(12);
    let rect = Shape::Rect { width: 17, height: 23 };
    let single = Single::Value(51);

    let tagged_some = Some(42_u32);
    let tagged_none: Option<u32> = None;
    let number = -25_i32;
    let ref_some = Some(&number);
    let ref_none: Option<&i32> = None;
    let box_some = Some(Box::new(88_u32));
    let box_none: Option<Box<u32>> = None;
    let nonzero_some = NonZeroU32::new(31);
    let nonzero_none: Option<NonZeroU32> = None;
    let offset_some = Some(Niche {
        prefix: 0,
        value: NonZeroU32::new(56).unwrap(),
    });
    let offset_none: Option<Niche> = None;
    let success: Result<u16, i32> = Ok(61);
    let failure: Result<u16, i32> = Err(-71);

    black_box(&circle); // BREAK: inspect
    black_box((&code_error, &code_ready, &point, &rect, &single));
    black_box((&tagged_some, &tagged_none, &ref_some, &ref_none, &box_some, &box_none));
    black_box((&nonzero_some, &nonzero_none, &offset_some, &offset_none, &success, &failure));
}
