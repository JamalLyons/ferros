//! Loop counter fixture program.
//!
//! Functions:
//! - `main`
//! - `tick`
//!
//! Markers:
//! - `tick_body`
//! - `loop_call`

#[inline(never)]
fn tick(counter: u64) -> u64
{
    let current = std::hint::black_box(counter); // BREAK: tick_body
    std::hint::black_box(current.wrapping_add(1))
}

fn main()
{
    let count = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u64>().ok())
        .unwrap_or(3000);
    let mut counter = 0;
    for _ in 0..count {
        counter = tick(counter); // BREAK: loop_call
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("final count: {counter}");
}
