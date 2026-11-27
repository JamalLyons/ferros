//! Statics for `print` and `globals`: an atomic the program changes, a `&str`, a
//! `static mut` array, a static in a module, one inside a function and a thread-local.
//!
//! The workspace forbids `unsafe`, so the `static mut` keeps its initial value: taking
//! its address with `&raw const` is safe and keeps it in the binary.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(1);
static NAME: &str = "ferris";
static mut TABLE: [u32; 4] = [1, 2, 3, 4];

mod config
{
    pub static LIMIT: u16 = 512;
}

thread_local! {
    static PER_THREAD: Cell<u32> = const { Cell::new(9) };
}

#[inline(never)]
fn calls() -> u8
{
    static CALLS: u8 = 7;
    black_box(CALLS)
}

fn main()
{
    COUNTER.fetch_add(41, Ordering::SeqCst);
    // The runtime address, slide included, for the test to compare with the debugger's.
    println!("COUNTER at {:p}", &raw const COUNTER);
    PER_THREAD.with(|value| value.set(10));
    black_box((&NAME, &raw const TABLE, config::LIMIT, calls()));
    black_box(&COUNTER); // BREAK: inspect
    black_box(PER_THREAD.with(Cell::get));
}
