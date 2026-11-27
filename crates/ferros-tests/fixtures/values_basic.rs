use std::hint::black_box;

#[inline(never)]
fn callee(int_arg: i32, float_arg: f64, bool_arg: bool, char_arg: char)
{
    let int_local = black_box(-17_i32);
    let float_local = black_box(1.25_f64);
    let bool_local = black_box(false);
    let char_local = black_box('λ');
    let array_local = black_box([0x10_u8, 0x32, 0x54, 0x76]);
    black_box((int_local, float_local, bool_local, char_local, array_local)); // BREAK: callee_locals
    {
        let later_local = black_box(101_u32);
        black_box(later_local); // BREAK: later_local
    }
    black_box((int_arg, float_arg, bool_arg, char_arg));
}

#[inline(never)]
fn caller(caller_int_arg: i32, caller_float_arg: f64, caller_bool_arg: bool, caller_char_arg: char)
{
    let caller_int_local = black_box(57_i32);
    let caller_float_local = black_box(-2.5_f64);
    let caller_bool_local = black_box(true);
    let caller_char_local = black_box('🦀');
    callee(37, 6.5, true, 'Ƶ'); // BREAK: caller_call
    let later_caller_local = black_box(102_u32);
    black_box(later_caller_local);
    black_box((caller_int_arg, caller_float_arg, caller_bool_arg, caller_char_arg));
    black_box((caller_int_local, caller_float_local, caller_bool_local, caller_char_local));
}

#[expect(clippy::inline_always, reason = "the fixture needs an inline frame with its own variables")]
#[inline(always)]
fn inline_values(inline_arg: u32)
{
    let inline_local = black_box(23_u32);
    black_box(inline_local); // BREAK: in_inline
    black_box(inline_arg);
}

#[inline(never)]
fn inline_owner()
{
    let owner_local = black_box(31_u32);
    inline_values(19);
    black_box(owner_local);
}

struct Inner
{
    count: i32,
}

struct Nested
{
    ready: bool,
    inner: Inner,
}

#[inline(never)]
fn formatted_samples()
{
    let nested = black_box(Nested {
        ready: true,
        inner: Inner { count: -4 },
    });
    let tuple_local = black_box(('λ', -3_i16, false));
    let array_local = black_box([-2_i16, 0, 7]);
    let number = black_box(-17_i32);
    let reference = black_box(&number);
    let double_reference = black_box(&reference);
    let null_raw = black_box(std::ptr::null::<i32>());
    let large_array = black_box([42_u8; 32]);
    black_box(&nested); // BREAK: format_locals
    black_box((nested.ready, nested.inner.count));
    black_box((tuple_local, array_local, double_reference, null_raw, large_array));
}

fn main()
{
    caller(-82, -0.75, false, '雪');
    inline_owner();
    formatted_samples();
}
