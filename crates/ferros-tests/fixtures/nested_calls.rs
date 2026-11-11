//! Nested calls fixture program.
//!
//! Functions:
//! - `main`
//! - `level_a`
//! - `level_b`
//! - `level_c`
//!
//! Markers:
//! - `in_a`
//! - `in_b`
//! - `in_c`
//! - `after_calls`

#[inline(never)]
fn level_c(val: u64) -> u64
{
    let input = std::hint::black_box(val); // BREAK: in_c
    std::hint::black_box(input.wrapping_add(3))
}

#[inline(never)]
fn level_b(val: u64) -> u64
{
    let input = std::hint::black_box(val); // BREAK: in_b
    let result = level_c(input.wrapping_add(2));
    std::hint::black_box(result)
}

#[inline(never)]
fn level_a(val: u64) -> u64
{
    let input = std::hint::black_box(val); // BREAK: in_a
    let result = level_b(input.wrapping_add(1));
    std::hint::black_box(result)
}

fn main()
{
    let res = level_a(40);
    let final_res = std::hint::black_box(res); // BREAK: after_calls
    println!("nested result: {final_res}");
}
