//! Recursion fixture program.
//!
//! `descend` calls itself until the depth given by the first argument (default 60) is
//! reached, so a stop at the bottom has at least that many frames above it.
//!
//! Functions:
//! - `main`
//! - `descend`
//!
//! Markers:
//! - `bottom`
//! - `recurse`

#[inline(never)]
fn descend(remaining: u64) -> u64
{
    if remaining == 0 {
        return std::hint::black_box(7); // BREAK: bottom
    }
    let below = descend(std::hint::black_box(remaining - 1)); // BREAK: recurse
    std::hint::black_box(below.wrapping_add(1)) // BREAK: after_recurse
}

fn main()
{
    let depth = std::env::args().nth(1).and_then(|arg| arg.parse::<u64>().ok()).unwrap_or(60);
    let result = descend(depth);
    println!("recursion result: {result}");
}
