//! Loop counter fixture program.
//!
//! Functions:
//! - `main`
//! - `tick`
//!
//! Markers:
//! - `tick_body`
//! - `loop_call`
//!
//! Pass `<count> --exit-gate <file>` (any count) to wait for the test to write to the file
//! before exiting.

#[inline(never)]
fn tick(counter: u64) -> u64
{
    let current = std::hint::black_box(counter); // BREAK: tick_body
    std::hint::black_box(current.wrapping_add(1))
}

// Keep source markers on the branch expressions that DWARF attributes to each stop.
#[rustfmt::skip]
fn main()
{
    let count = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u64>().ok())
        .unwrap_or(3000);
    let mut options = std::env::args().skip(2);
    let option = options.next();
    let progress = option.as_deref() == Some("--progress");
    if option.as_deref() == Some("--exit-gate") {
        let path = options.next().expect("exit gate path");
        while std::fs::metadata(&path).expect("read exit gate").len() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let mut counter = 0;
    for _ in 0..count { // BREAK: loop_header
        counter = tick(counter); // BREAK: loop_call
        if progress { // BREAK: loop_progress
            println!("tick: {counter}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10)); // BREAK: loop_sleep
    }
    println!("final count: {counter}"); // BREAK: loop_final
}
