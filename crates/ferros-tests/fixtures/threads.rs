//! Threads fixture program.
//!
//! An optional first argument is a number of milliseconds to keep running after both
//! workers have exited (default 0), for tests that need a live process whose worker
//! threads are gone. A second optional argument sets the worker sleep in milliseconds
//! (default 50), for tests that need workers alive during attach.
//!
//! Functions:
//! - `main`
//! - `worker_body`
//!
//! Markers:
//! - `worker_body`
//! - `after_join`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

static WORKER_SLEEP_MS: AtomicU64 = AtomicU64::new(50);

#[inline(never)]
fn worker_body(id: u64) -> u64
{
    let input = std::hint::black_box(id); // BREAK: worker_body
    thread::sleep(Duration::from_millis(WORKER_SLEEP_MS.load(Ordering::Relaxed)));
    std::hint::black_box(input.wrapping_mul(10))
}

fn main()
{
    if let Some(ms) = std::env::args().nth(2).and_then(|arg| arg.parse::<u64>().ok()) {
        WORKER_SLEEP_MS.store(ms, Ordering::Relaxed);
    }
    let barrier = Arc::new(Barrier::new(2));

    let b1 = Arc::clone(&barrier);
    let handle1 = thread::Builder::new()
        .name("fixture-worker-1".to_string())
        .spawn(move || {
            b1.wait();
            worker_body(1)
        })
        .expect("failed to spawn fixture-worker-1");

    let b2 = Arc::clone(&barrier);
    let handle2 = thread::Builder::new()
        .name("fixture-worker-2".to_string())
        .spawn(move || {
            b2.wait();
            worker_body(2)
        })
        .expect("failed to spawn fixture-worker-2");

    let res1 = handle1.join().expect("worker 1 panicked");
    let res2 = handle2.join().expect("worker 2 panicked");
    let sum = std::hint::black_box(res1.wrapping_add(res2)); // BREAK: after_join
    println!("threads finished: {sum}");
    let hold_ms = std::env::args().nth(1).and_then(|arg| arg.parse::<u64>().ok()).unwrap_or(0);
    thread::sleep(Duration::from_millis(hold_ms));
}
