//! One of two `twin.rs` files in the `symbols` fixture, each defining `twin`: the file
//! name and the function name are ambiguous on purpose, whatever the toolchain's standard
//! library source layout is.

#[inline(never)]
pub fn twin(value: u64) -> u64
{
    std::hint::black_box(value.wrapping_add(1))
}
