//! Calls that `step` must enter although looking their callee up by name fails.
//!
//! `Gate<'<'>` and `Gate<'>'>` carry a bracket character as a const generic argument, so
//! their demangled method names (`<Gate<'<'> as Bump>::bump`) do not parse as paths.
//!
//! - `<Gate<'<'> as Bump>::bump`, a trait-impl method called through a `dyn Bump` (marker
//!   `call_trait`, first line in the callee `in_trait`).
//! - A closure defined in `<Gate<'>'>>::run` and called through `&dyn Fn` (marker
//!   `call_closure`, first line in the callee `in_closure`).

trait Bump
{
    fn bump(&mut self, by: u64) -> u64;
}

struct Gate<const MARK: char>(u64);

impl Bump for Gate<'<'>
{
    #[inline(never)]
    fn bump(&mut self, by: u64) -> u64
    {
        self.0 = self.0.wrapping_add(std::hint::black_box(by)); // BREAK: in_trait
        self.0
    }
}

impl Gate<'>'>
{
    #[inline(never)]
    fn run(&self, value: u64) -> u64
    {
        let offset = self.0;
        let closure = move |value: u64| {
            let doubled = std::hint::black_box(value).wrapping_mul(2); // BREAK: in_closure
            doubled.wrapping_add(offset)
        };
        call(&closure, value)
    }
}

#[inline(never)]
fn call(f: &dyn Fn(u64) -> u64, value: u64) -> u64
{
    f(value) // BREAK: call_closure
}

#[inline(never)]
fn run_trait(target: &mut dyn Bump, by: u64) -> u64
{
    target.bump(by) // BREAK: call_trait
}

fn main()
{
    let mut open = Gate::<'<'>(1);
    let a = run_trait(&mut open, 2);
    let b = Gate::<'>'>(7).run(a);
    println!("step targets: {a} {b}");
}
