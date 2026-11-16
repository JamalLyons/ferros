//! Nested calls fixture program.
//!
//! Functions:
//! - `main`
//! - `level_a`
//! - `level_b`
//! - `level_c`
//! - `leaf_mix` (frameless leaf, see its comment)
//! - `exit_failure`, `exit_success` (diverging, see their comment)
//!
//! Markers:
//! - `in_a`
//! - `in_b`
//! - `in_c`
//! - `call_c` (the call of `level_c` in `level_b`)
//! - `call_b` (the call of `level_b` in `level_a`)
//! - `call_a` (the call of `level_a` in `main`)
//! - `after_calls`
//! - `in_leaf` (inside `leaf_mix`)
//! - `call_leaf` (the call of `leaf_mix` in `main`)
//! - `call_exit` (the call of `exit_success` in `main`)

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
    let result = level_c(input.wrapping_add(2)); // BREAK: call_c
    std::hint::black_box(result)
}

#[inline(never)]
fn level_a(val: u64) -> u64
{
    let input = std::hint::black_box(val); // BREAK: in_a
    let result = level_b(input.wrapping_add(1)); // BREAK: call_b
    std::hint::black_box(result)
}

static LEAF_SEED: u64 = 0x1234_5678;
static LEAF_MASK: u64 = 0x5a;

/// A leaf with no arguments, locals or calls. A debug build gives it no stack frame at
/// all, so its CFI is empty and the linker records it as compact-unwind
/// `UNWIND_ARM64_MODE_FRAMELESS` with no `__eh_frame` FDE. The return address stays in
/// `x30` and `fp` is still the caller's.
#[inline(never)]
fn leaf_mix() -> u64
{
    LEAF_SEED ^ LEAF_MASK // BREAK: in_leaf
}

/// Two adjacent diverging functions. A debug build gives each a frame record and no
/// `__eh_frame` FDE (compact unwind `UNWIND_ARM64_MODE_FRAME`), and the linker merges the
/// pair into one compact-unwind entry that starts at `exit_failure`: `exit_success` has no
/// entry of its own.
#[inline(never)]
fn exit_failure(code: i32) -> !
{
    eprintln!("nested failure");
    std::process::exit(code)
}

#[inline(never)]
fn exit_success(code: i32) -> !
{
    std::process::exit(code)
}

fn main()
{
    let res = level_a(40); // BREAK: call_a
    let final_res = std::hint::black_box(res); // BREAK: after_calls
    println!("nested result: {final_res}");
    let leaf = leaf_mix(); // BREAK: call_leaf
    println!("leaf result: {leaf}");
    if std::hint::black_box(leaf) == 0 {
        exit_failure(1);
    }
    exit_success(0); // BREAK: call_exit
}
