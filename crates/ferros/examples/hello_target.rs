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

    let test_value: u64 = 0xDEADBEEFCAFEBABE;

    // Print the address of the static variable for testing
    println!("TEST_VARIABLE address: {:p}", &test_value as *const u64);
    println!("TEST_VARIABLE value: 0x{:016x}", test_value);

    loop {
        // Keep the variable in scope and accessible
        let _keep_alive = test_value;

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
