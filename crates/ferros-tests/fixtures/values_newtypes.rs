//! Standard library newtypes and niche wrappers for the formatter integration test.

use std::hint::black_box;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::num::{NonZeroU32, Wrapping};
use std::time::Duration;

fn main()
{
    let non_zero = NonZeroU32::new(12).unwrap();
    let some_non_zero = NonZeroU32::new(12);
    let no_non_zero: Option<NonZeroU32> = None;
    let duration = Duration::new(1, 500_000_000);
    let wrapping = Wrapping(5_u8);
    let manually_drop = ManuallyDrop::new(6_u32);
    let maybe_uninit = MaybeUninit::new(7_u32);
    let thread_id = std::thread::current().id();
    let phantom: PhantomData<u32> = PhantomData;
    let ids = vec![1_u8, 2, 3];

    black_box(&non_zero); // BREAK: inspect
    black_box((&some_non_zero, &no_non_zero, &duration, &wrapping, &manually_drop));
    black_box((&maybe_uninit, &thread_id, &phantom, &ids));
}
