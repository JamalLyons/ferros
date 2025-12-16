//! Simple test target program for debugging with Ferros
//!
//! This is a minimal test program that doesn't require any external dependencies.
//! It runs in a loop and can be attached to with the debugger.

fn main()
{
    let pid = std::process::id();
    println!("Simple Test Target Program Starting...");
    println!("PID: {}", pid);
    println!("This program will run until interrupted");

    // Write PID to file for easy attachment
    // Try multiple possible paths depending on where the example is run from
    let pid_paths = [
        "crates/ferros/examples/pid.txt", // From workspace root
        "examples/pid.txt",               // From crate root
        "./pid.txt",                      // Current directory
    ];

    let mut written = false;
    for pid_file in &pid_paths {
        if let Ok(()) = std::fs::write(pid_file, pid.to_string()) {
            written = true;
            break;
        }
    }

    if !written {
        eprintln!("Warning: Failed to write PID to any expected location");
    }
    loop {
        // Simulate some work
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
