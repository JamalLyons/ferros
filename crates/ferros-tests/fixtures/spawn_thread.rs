//! Starts one worker before a test breakpoint and another afterward.
//!
//! The second worker is a normal `std::thread`, so it enters `libsystem_pthread` at
//! `_thread_start`. `pause=<milliseconds>` sleeps after the first worker is running and
//! before that second thread is created, so a test can attach in between.
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

#[inline(never)]
fn spawn_later()
{
    let child = thread::spawn(|| {
        println!("new worker ran");
        thread::sleep(Duration::from_secs(2));
    });
    child.join().expect("new worker");
}

fn main()
{
    let barrier = Arc::new(Barrier::new(2));
    let other = Arc::clone(&barrier);
    let worker = thread::spawn(move || {
        other.wait();
        thread::sleep(Duration::from_secs(2));
    });
    barrier.wait();
    for arg in std::env::args().skip(1) {
        if let Some(milliseconds) = arg.strip_prefix("pause=")
            && let Ok(milliseconds) = milliseconds.parse::<u64>()
            && milliseconds > 0
        {
            thread::sleep(Duration::from_millis(milliseconds));
        }
    }
    spawn_later();
    worker.join().expect("worker");
    println!("workers finished");
}
