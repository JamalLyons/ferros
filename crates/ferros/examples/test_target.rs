//! Test target program for debugging with Ferros
//!
//! This program is designed to be debugged and provides various features
//! to test the debugger:
//! - Long-running loop
//! - Function calls with parameters
//! - Variable manipulation
//! - Memory allocation
//! - Thread spawning
//! - Signal handling

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

fn main()
{
    println!("Test Target Program Starting...");
    println!("PID: {}", std::process::id());
    println!("Press Ctrl+C to exit gracefully");

    // Set up signal handler
    ctrlc::set_handler(|| {
        println!("\nReceived interrupt signal, shutting down...");
        RUNNING.store(false, Ordering::Relaxed);
    })
    .expect("Error setting signal handler");

    // Spawn a worker thread
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let worker = thread::spawn(move || {
        worker_thread(running_clone);
    });

    // Main loop with function calls
    let mut counter = 0u64;
    let mut accumulator = 0i64;

    while RUNNING.load(Ordering::Relaxed) {
        counter += 1;
        accumulator = process_value(counter, accumulator);

        if counter % 100 == 0 {
            println!("Counter: {}, Accumulator: {}", counter, accumulator);
        }

        // Allocate some memory periodically
        if counter % 50 == 0 {
            let _vec = vec![0u8; 1024];
            let _string = format!("Allocated memory at iteration {}", counter);
            // Memory is dropped here
        }

        thread::sleep(Duration::from_millis(100));
    }

    // Clean shutdown
    running.store(false, Ordering::Relaxed);
    worker.join().expect("Worker thread panicked");

    println!("Test Target Program Exiting...");
    println!("Final counter: {}, Final accumulator: {}", counter, accumulator);
}

/// Process a value and return updated accumulator
fn process_value(value: u64, acc: i64) -> i64
{
    let result = if value % 2 == 0 {
        acc + (value as i64)
    } else {
        acc - (value as i64)
    };

    // Nested function call
    validate_result(result)
}

/// Validate the result is within reasonable bounds
fn validate_result(result: i64) -> i64
{
    if result > 1_000_000 {
        result % 1_000_000
    } else if result < -1_000_000 {
        -((-result) % 1_000_000)
    } else {
        result
    }
}

/// Worker thread that does some computation
fn worker_thread(running: Arc<AtomicBool>)
{
    let mut local_counter = 0u32;

    while running.load(Ordering::Relaxed) {
        local_counter = local_counter.wrapping_add(1);

        // Do some computation
        let _fib = fibonacci(local_counter % 30);

        thread::sleep(Duration::from_millis(200));
    }

    println!("Worker thread exiting, processed {} iterations", local_counter);
}

/// Calculate Fibonacci number (inefficient recursive version for testing)
fn fibonacci(n: u32) -> u64
{
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci(n - 1) + fibonacci(n - 2),
    }
}
