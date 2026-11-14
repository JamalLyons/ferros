//! Starts one worker before a test breakpoint and another afterward.
//!
//! The second worker is a normal `std::thread`, so it enters `libsystem_pthread` at
//! `_thread_start`. `pause=<milliseconds>` sleeps after the first worker is running and
//! before `spawn_later`. `spawn_later` itself waits, then starts one more worker from
//! `spawn_second`, so a test can stop between those two creates.
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

#[inline(never)]
fn spawn_second()
{
    let child = thread::spawn(|| {
        println!("second worker ran");
        thread::sleep(Duration::from_secs(2));
    });
    child.join().expect("second worker");
}

#[inline(never)]
fn spawn_later()
{
    let child = thread::spawn(|| {
        println!("new worker ran");
        thread::sleep(Duration::from_secs(2));
    });
    // Longer than the birth-hold quiet period, so that wait observes only the
    // first thread. The step-off test breaks on `spawn_second` instead.
    thread::sleep(Duration::from_millis(1500));
    spawn_second();
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
