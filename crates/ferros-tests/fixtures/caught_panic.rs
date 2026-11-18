//! A call that unwinds past its caller: `inner` panics (marker `in_inner`), `middle`
//! (marker `call_inner`) never gets the call back, and `main`'s `catch_unwind` catches
//! it. `after_catch` is the first line after `catch_unwind` returns.

#[inline(never)]
fn inner(value: u64) -> u64
{
    if std::hint::black_box(value) > 0 {
        std::panic::panic_any(value); // BREAK: in_inner
    }
    value
}

#[inline(never)]
fn middle(value: u64) -> u64
{
    let result = inner(value); // BREAK: call_inner
    std::hint::black_box(result)
}

fn main()
{
    // The default hook prints the panic; keep stderr quiet.
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| middle(std::hint::black_box(5)));
    let failed = std::hint::black_box(caught.is_err()); // BREAK: after_catch
    println!("caught panic: {failed}");
}
