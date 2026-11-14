//! Source-lookup fixture program: line tables and function names, read without a process.
//!
//! Functions:
//! - `main`
//! - `generic_identity` (generic; instantiated for `u64` and `u32`)
//! - `Counter::bump` (inherent method)
//! - `shapes::area` (inside a module)
//! - `always_inlined` (`#[inline(always)]`; inlined into `main` at two call sites)
//! - `with_gap` (a comment-only line between two code lines, just before `after_gap`)
//!
//! Markers:
//! - `in_generic`
//! - `in_method`
//! - `in_module`
//! - `in_inlined`
//! - `before_gap`
//! - `after_gap`

#[inline(never)]
fn generic_identity<T: Copy>(value: T) -> T
{
    std::hint::black_box(value) // BREAK: in_generic
}

struct Counter
{
    count: u64,
}

impl Counter
{
    #[inline(never)]
    fn bump(&mut self) -> u64
    {
        self.count = std::hint::black_box(self.count.wrapping_add(1)); // BREAK: in_method
        self.count
    }
}

mod shapes
{
    #[inline(never)]
    pub fn area(width: u64, height: u64) -> u64
    {
        std::hint::black_box(width.wrapping_mul(height)) // BREAK: in_module
    }
}

#[expect(clippy::inline_always, reason = "the fixture needs inlined copies of a function")]
#[inline(always)]
fn always_inlined(value: u64) -> u64
{
    std::hint::black_box(value.wrapping_add(7)) // BREAK: in_inlined
}

#[inline(never)]
fn with_gap(value: u64) -> u64
{
    let first = std::hint::black_box(value); // BREAK: before_gap
    // This line has no code: a lookup here resolves to `after_gap`.
    std::hint::black_box(first.wrapping_add(1)) // BREAK: after_gap
}

fn main()
{
    let wide = generic_identity(std::hint::black_box(1_u64));
    let narrow = generic_identity(std::hint::black_box(2_u32));
    let mut counter = Counter { count: 0 };
    let bumped = counter.bump();
    let area = shapes::area(3, 4);
    let first = always_inlined(std::hint::black_box(10));
    let second = always_inlined(std::hint::black_box(first));
    let gap = with_gap(5);
    let total = wide
        .wrapping_add(u64::from(narrow))
        .wrapping_add(bumped)
        .wrapping_add(area)
        .wrapping_add(second)
        .wrapping_add(gap);
    println!("symbols: {total}");
}
