use std::hint::black_box;

#[inline(never)]
fn callee(int_arg: i32, float_arg: f64, bool_arg: bool, char_arg: char)
{
    let int_local = black_box(-17_i32);
    let float_local = black_box(1.25_f64);
    let bool_local = black_box(false);
    let char_local = black_box('λ');
    black_box((int_local, float_local, bool_local, char_local)); // BREAK: callee_locals
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

fn main()
{
    caller(-82, -0.75, false, '雪');
    inline_owner();
}
