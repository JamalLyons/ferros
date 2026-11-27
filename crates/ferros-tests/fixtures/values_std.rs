//! Standard library values for the formatter integration test.

use std::hint::black_box;

pub struct Point
{
    pub x: i32,
    pub y: i32,
}

fn main()
{
    const LONG: &[i64] = &[42; 20];
    let text = "café\n\"quoted\"";
    let empty = String::new();
    let owned = String::from("naïve\n");
    let mut bytes = Vec::with_capacity(8);
    bytes.extend([1_u8, 2, 3]);
    let strings = vec![String::from("one"), String::from("two")];
    let slice: &[i64] = &[-2, 5, 8];
    let long_slice = LONG;
    let empty_slice: &[i64] = &[];
    let point = Box::new(Point { x: -7, y: 12 });
    let boxed_array = Box::new([3_u8, 5]);
    let boxed_slice: Box<[u8]> = Box::new([6, 7]);
    let boxed_str: Box<str> = "boxed".into();
    let units = vec![(); 3];
    // The first byte beyond the preview limit completes a multibyte character.
    let long_string = format!("{}é{}", "a".repeat(255), "z".repeat(32));
    let long_text = long_string.as_str();

    black_box(&text); // BREAK: inspect
    black_box((&empty, &owned, &bytes, &strings, &slice, &long_slice, &empty_slice));
    black_box((
        &point,
        &boxed_array,
        &boxed_slice,
        &boxed_str,
        &units,
        &long_string,
        &long_text,
    ));
}
