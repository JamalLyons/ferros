//! Starts one worker before a test breakpoint and another afterward.
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
    spawn_later();
    worker.join().expect("worker");
    println!("workers finished");
}
