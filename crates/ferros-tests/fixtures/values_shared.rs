//! Shared pointers and cells for the formatter integration test.

use std::cell::{Cell, RefCell};
use std::hint::black_box;
use std::rc::Rc;
use std::sync::Arc;

fn main()
{
    let rc = Rc::new(String::from("shared"));
    let rc_clone = Rc::clone(&rc);
    let weak = Rc::downgrade(&rc);
    let arc = Arc::new(vec![7_u32, 8, 9]);
    let arc_weak = Arc::downgrade(&arc);
    let rc_slice: Rc<[u8]> = Rc::new([4, 8]);
    let arc_str: Arc<str> = Arc::from("café");

    let cell = Cell::new(Some(7_u16));
    let idle = RefCell::new(-6_i32);
    let shared = RefCell::new(15_i32);
    let first = shared.borrow();
    let second = shared.borrow();
    let exclusive = RefCell::new(22_i32);
    let mut guard = exclusive.borrow_mut();
    *guard = 42;

    black_box(&rc); // BREAK: inspect
    black_box((&rc_clone, &weak, &arc, &arc_weak, &rc_slice, &arc_str));
    black_box((&cell, &idle, &shared, &first, &second, &exclusive, &guard));
}
