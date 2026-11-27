//! Unions, function pointers, raw slice and `str` pointers, and vector and array iterators
//! for the value formatter integration test (issue #412).
#![expect(dead_code, reason = "the debugger reads these fields; the program does not")]

use std::hint::black_box;

#[repr(C)]
union Bits
{
    int: u32,
    float: f32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Halves
{
    low: u16,
    high: u16,
}

#[repr(C)]
union Word
{
    halves: Halves,
    raw: u32,
    bytes: [u8; 4],
}

// Writing one member is safe; reading another as a function pointer is what the debugger does.
#[repr(C)]
union Callback
{
    raw: usize,
    call: fn(u32, u32) -> u32,
}

struct Handler
{
    on_event: fn(u32, u32) -> u32,
}

fn add(a: u32, b: u32) -> u32
{
    a.wrapping_add(b)
}

fn main()
{
    let bits = Bits { int: 1 };
    let word = Word { raw: 0x0002_0001 };
    let callback: fn(u32, u32) -> u32 = add;
    let handler = Handler { on_event: add };
    // Inside `add` but not at its entry, so no function starts there.
    let stray = Callback {
        raw: callback as usize + 4,
    };
    let data = [7_u8, 9];
    let data_ref = &data;
    let raw_slice: *const [u8] = &raw const data;
    let null_slice: *mut [u8] = std::ptr::slice_from_raw_parts_mut(std::ptr::null_mut(), 0);
    let mut remaining = vec![1_u32, 2, 3].into_iter();
    let first = remaining.next();
    let empty = Vec::<u64>::new().into_iter();
    let mut array_iter = [1_u8, 2, 3].into_iter();
    let array_first = array_iter.next();
    let uninit = std::mem::MaybeUninit::new(0x0102_0304_u32);
    let text = "hi there";
    let raw_str: *const str = text;
    let raw_mut_str: *mut str = std::ptr::from_ref::<str>(text).cast_mut();

    black_box((&bits, &word, &stray, &handler, &raw_slice, &null_slice, &data_ref)); // BREAK: inspect
    black_box((callback(1, 2), &remaining, &first, &empty));
    black_box((&array_iter, &array_first, &uninit, &raw_str, &raw_mut_str));
}
