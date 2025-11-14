/// Simple target program for debugging with ptrace.
///
/// This program runs in a loop, printing its PID and a counter.
/// It's designed to be attached to by a debugger using ptrace.
///
/// The program will run indefinitely until interrupted (Ctrl+C) or killed,
/// giving you plenty of time to attach the debugger.
fn main()
{
    let pid = std::process::id();
    println!("Hello! I am process {pid}");
    println!("Waiting for debugger to attach... (Press Ctrl+C to exit)");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
