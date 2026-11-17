//! Threads that call one function in a tight loop, with no sleep.
//!
//! The first argument is the number of spinning threads, main included (default 8).
//! Every thread keeps `spin_site` hot in the instruction cache of the core it runs on,
//! so a test can check that the code the debugger writes is the code those threads
//! execute next. Before spinning, the fixture prints `calls=<address>`: the address of
//! the counter that `spin_site` increments once per call.
//!
//! With the first argument `one-line`, only the main thread runs, in a loop written on a
//! single source line (`spin_line`), so a source `next` there never reaches another line.
//!
//! Functions:
//! - `spin_site`
//! - `spin_on_one_line`
//!
//! Markers:
//! - `spin_line`

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

static CALLS: AtomicU64 = AtomicU64::new(0);

#[inline(never)]
fn spin_site()
{
    CALLS.fetch_add(1, Ordering::Relaxed);
}

fn spin() -> !
{
    loop {
        spin_site();
    }
}

#[rustfmt::skip]
#[inline(never)]
fn spin_on_one_line() -> !
{
    loop { std::hint::black_box(CALLS.load(Ordering::Relaxed)); spin_site(); } // BREAK: spin_line
}

fn main()
{
    if std::env::args().nth(1).as_deref() == Some("one-line") {
        spin_on_one_line();
    }
    let threads = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(8)
        .max(1);
    println!("calls={:p}", &raw const CALLS);
    std::io::stdout().flush().expect("flush stdout");
    for _ in 1..threads {
        std::thread::spawn(|| spin());
    }
    spin();
}
