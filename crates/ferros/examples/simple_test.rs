//! Simple test target program for debugging with Ferros
//!
//! This is a minimal test program that doesn't require any external dependencies.
//! It runs in a loop and can be attached to with the debugger.

fn main()
{
    println!("Simple Test Target Program Starting...");
    println!("PID: {}", std::process::id());
    println!("This program will run until interrupted");

    let mut counter = 0u64;
    let mut sum = 0i64;

    loop {
        counter += 1;
        sum = add_to_sum(counter, sum);

        if counter.is_multiple_of(50) {
            println!("Iteration: {}, Sum: {}", counter, sum);
        }

        // Simulate some work
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Allocate some memory
        if counter.is_multiple_of(25) {
            let _data = vec![counter; 100];
            let _message = format!("Allocated at iteration {}", counter);
        }
    }
}

fn add_to_sum(value: u64, current_sum: i64) -> i64
{
    let result = if value.is_multiple_of(2) {
        current_sum + (value as i64)
    } else {
        current_sum - (value as i64)
    };

    // Nested call
    clamp_result(result)
}

fn clamp_result(value: i64) -> i64
{
    value.clamp(-1000, 1000)
}
