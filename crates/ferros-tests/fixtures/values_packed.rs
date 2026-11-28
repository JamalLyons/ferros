//! Issue #415 probe. Keep the distinct generic instantiations that spread code across codegen units.

#![allow(dead_code)]
use std::borrow::Cow;
use std::cell::{OnceCell, UnsafeCell};
use std::collections::*;
use std::ffi::{CString, OsString};
use std::hint::black_box;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
#[derive(Clone, Copy)]
#[repr(C)]
union U
{
    a: u32,
    b: f32,
}
#[derive(Clone, Copy)]
enum Color
{
    Red,
    Green,
}
enum Shape
{
    Circle
    {
        r: f64,
    },
    Square(u32),
    Empty,
}
struct Unit;
struct Gen<T>
{
    v: T,
}
fn add(a: u32, b: u32) -> u32
{
    a + b
}
fn main()
{
    let vd: VecDeque<u32> = (1_u32..=4).collect::<VecDeque<u32>>();
    let ll: LinkedList<u8> = [1_u8, 2].into_iter().collect::<LinkedList<u8>>();
    let bh: BinaryHeap<u32> = [3_u32, 1, 2].into_iter().collect::<BinaryHeap<u32>>();
    let bts: BTreeSet<u32> = [5, 6].into_iter().collect();
    let mutex = Mutex::new(9_u32);
    let rw = RwLock::new(8_u32);
    let atomic = AtomicU32::new(7);
    let ab = AtomicBool::new(true);
    let once: OnceCell<u32> = OnceCell::new();
    once.set(4).unwrap();
    let uc = UnsafeCell::new(3_u32);
    let nz = NonZeroU32::new(12).unwrap();
    let cs = CString::new("cstr").unwrap();
    let os = OsString::from("os");
    let pb = PathBuf::from("/tmp/x");
    let cow_b: Cow<str> = Cow::Borrowed("cow");
    let cow_o: Cow<str> = Cow::Owned(String::from("owned"));
    let dur = Duration::from_millis(1500);
    let pinned = Box::pin(5_u32);
    let u = U { a: 1 };
    let color = Color::Green;
    let shape = Shape::Circle { r: 1.5 };
    let sq = Shape::Square(3);
    let unit = Unit;
    let g = Gen { v: 2_u8 };
    let fptr: fn(u32, u32) -> u32 = add;
    let closure_cap = 10_u32;
    let closure = move |x: u32| x + closure_cap;
    let s128: i128 = -1 << 100;
    let u128v: u128 = 1 << 100;
    let f32v = 1.5_f32;
    let opt_str: Option<&str> = Some("hi");
    let res: Result<u32, String> = Err(String::from("bad"));
    let nested_opt: Option<Option<u8>> = Some(None);
    let rc_cell = Rc::new(std::cell::RefCell::new(vec![1_u8]));
    let raw_slice: *const [u8] = &[1u8, 2][..];
    let char_arr = ['a', 'b'];
    let tup0 = ();
    let iter = vec![1, 2, 3].into_iter();
    let range = 1..5_u32;
    let range_inc = 1..=5_u32;
    let ord = std::cmp::Ordering::Less;
    let hm_big: HashMap<String, Vec<u32>> = [("a".to_string(), vec![1])].into_iter().collect();
    let string_ref: &String = &cs.to_str().unwrap().to_string();
    let bstr: &[u8] = b"bytes";
    let slice2d: &[[u8; 2]] = &[[1, 2], [3, 4]];
    let wrapping = std::num::Wrapping(5_u8);
    let phantom = std::marker::PhantomData::<u32>;
    let mdrop = std::mem::ManuallyDrop::new(6_u32);
    let mu = std::mem::MaybeUninit::new(7_u32);
    let weak = Rc::downgrade(&rc_cell);
    let thread_id = std::thread::current().id();
    black_box(&vd); // BREAK: inspect
    black_box((
        &ll,
        &bh,
        &bts,
        &mutex,
        &rw,
        &atomic,
        &ab,
        &once,
        &uc,
        &nz,
        &cs,
        &os,
        &pb,
        &cow_b,
        &cow_o,
        &dur,
        &pinned,
        &u,
        &color,
        &shape,
        &sq,
        &unit,
        &g,
        &fptr,
        &closure,
        &s128,
        &u128v,
        &f32v,
        &opt_str,
        &res,
        &nested_opt,
        &rc_cell,
        &raw_slice,
        &char_arr,
        &tup0,
        &iter,
        &range,
        &range_inc,
        &ord,
        &hm_big,
        &string_ref,
        &bstr,
        &slice2d,
        &wrapping,
        &phantom,
        &mdrop,
        &mu,
        &weak,
        &thread_id,
    ));
}
