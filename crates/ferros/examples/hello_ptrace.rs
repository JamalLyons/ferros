//! macOS debugger example using ferros-core library.
//!
//! This demonstrates Project 1.1: Process attachment and register reading
//! using the ferros-core library abstraction instead of raw Mach APIs.
//!
//! ## What this example does:
//!
//! 1. Attaches to a running process using `MacOSDebugger::attach()`
//! 2. Reads registers using `debugger.read_registers()`
//! 3. Displays the register values in a readable format
//!
//! ## Usage:
//!
//! ```bash
//! # Terminal 1: Start target process
//! cargo run --example hello_target
//!
//! # Terminal 2: Attach debugger (note the PID from terminal 1)
//! sudo cargo run --example hello_ptrace <PID>
//! ```
//!
//! ## Note:
//!
//! Requires sudo or debugging entitlements due to macOS security restrictions.
//! See `MACOS_GUIDE.md` for details on setting up entitlements.

#[cfg(not(target_os = "macos"))]
fn main()
{
    eprintln!("This example is macOS-only. It uses ferros-core's macOS implementation which requires Mach APIs.");
    eprintln!("Please run this example on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod macos_impl
{
    use std::process;

    use ferros_core::platform::macos::MacOSDebugger;
    use ferros_core::types::ProcessId;
    use ferros_core::Debugger;

    pub fn main()
    {
        let args: Vec<String> = std::env::args().collect();

        if args.len() != 2 {
            eprintln!("Usage: {} <PID>", args[0]);
            eprintln!("Example: {} 12345", args[0]);
            eprintln!("\nFirst, run: cargo run --example hello_target");
            eprintln!("Then run with sudo: sudo cargo run --example hello_ptrace <PID>");
            process::exit(1);
        }

        let pid: u32 = match args[1].parse() {
            Ok(pid) => pid,
            Err(e) => {
                eprintln!("Error: Invalid PID '{}': {}", args[1], e);
                process::exit(1);
            }
        };

        println!("🔍 Ferros macOS Debugger Example");
        println!("==================================");
        println!("Platform: macOS (using ferros-core)");
        println!("Attempting to attach to process {pid}...\n");

        // First, verify the process exists
        if !process_exists(pid) {
            eprintln!("❌ Error: Process {pid} does not exist or has exited");
            eprintln!();
            eprintln!("💡 Troubleshooting:");
            eprintln!("   - Verify the PID is correct: ps aux | grep {}", pid);
            eprintln!("   - Make sure hello_target is still running");
            eprintln!("   - Run: cargo run --example hello_target");
            process::exit(1);
        }

        println!("✅ Process {pid} exists, attempting attachment...\n");

        // Use the ferros-core library instead of raw Mach APIs
        match attach_and_read_registers(pid) {
            Ok(()) => {
                println!("\n✅ Successfully read registers from process {pid}");
            }
            Err(e) => {
                eprintln!("❌ Error: {}", e);
                eprintln!();

                // Match on the error enum variants
                // This gives us:
                // - Type safety: compiler ensures we handle all cases
                // - Performance: no string conversion overhead
                // - Precision: exact match instead of substring matching
                match e {
                    // Permission denied errors
                    ferros_core::error::DebuggerError::PermissionDenied(_)
                    | ferros_core::error::DebuggerError::MachError(
                        ferros_core::platform::macos::error::MachError::ProtectionFailure,
                    ) => {
                        eprintln!("🔒 Permission Denied");
                        eprintln!();
                        eprintln!("This is macOS's security system blocking task_for_pid().");
                        eprintln!("You have two options:");
                        eprintln!();
                        eprintln!("Option 1: Run with sudo (Quickest)");
                        eprintln!("   sudo cargo run --example hello_ptrace {pid}", pid = pid);
                        eprintln!("   (You'll be prompted for your password)");
                        eprintln!();
                        eprintln!("Option 2: Grant debugging entitlement (More secure)");
                        eprintln!("   1. Create debugger.entitlements file");
                        eprintln!(
                            "   2. Sign the binary: codesign --entitlements debugger.entitlements --force --sign - \
                             target/debug/examples/hello_ptrace"
                        );
                        eprintln!("   3. Run normally");
                        eprintln!();
                        eprintln!("See MACOS_GUIDE.md for detailed instructions.");
                    }

                    // Process not found errors
                    ferros_core::error::DebuggerError::ProcessNotFound(_)
                    | ferros_core::error::DebuggerError::MachError(
                        ferros_core::platform::macos::error::MachError::ProcessNotFound,
                    ) => {
                        eprintln!("💡 Troubleshooting:");
                        eprintln!("   - Verify the PID is correct: ps aux | grep {}", pid);
                        eprintln!("   - Make sure the process is still running");
                        eprintln!("   - The process may have exited");
                    }

                    // Invalid argument errors - extract message when available
                    ferros_core::error::DebuggerError::InvalidArgument(msg) => {
                        eprintln!("💡 Invalid Argument:");
                        eprintln!("   {}", msg);
                        eprintln!("   - Check that you provided a valid PID");
                        eprintln!("   - Make sure the process architecture matches");
                    }
                    ferros_core::error::DebuggerError::MachError(
                        ferros_core::platform::macos::error::MachError::InvalidArgument,
                    ) => {
                        eprintln!("💡 Invalid Argument:");
                        eprintln!("   - Check that you provided a valid PID");
                        eprintln!("   - Make sure the process architecture matches");
                    }

                    // Attach failed - extract message
                    ferros_core::error::DebuggerError::AttachFailed(msg) => {
                        eprintln!("💡 Attach Failed:");
                        eprintln!("   {}", msg);
                        eprintln!("   - Make sure the process is still running");
                        eprintln!("   - Try running with sudo if permission-related");
                    }

                    // Read registers failed - extract message
                    ferros_core::error::DebuggerError::ReadRegistersFailed(msg) => {
                        eprintln!("💡 Read Registers Failed:");
                        eprintln!("   {}", msg);
                        eprintln!("   - The thread may have exited");
                        eprintln!("   - Check architecture compatibility");
                    }

                    // Unknown Mach error
                    ferros_core::error::DebuggerError::MachError(
                        ferros_core::platform::macos::error::MachError::Unknown(code),
                    ) => {
                        eprintln!("💡 Unknown Mach Error (code: {}):", code);
                        eprintln!("   - Check the error message above");
                        eprintln!("   - This may be a newer macOS version with different error codes");
                    }

                    // I/O error
                    ferros_core::error::DebuggerError::Io(err) => {
                        eprintln!("💡 I/O Error:");
                        eprintln!("   {}", err);
                        eprintln!("   - Check file permissions");
                        eprintln!("   - Ensure sufficient disk space");
                    }
                }
                process::exit(1);
            }
        }
    }

    /// Check if a process with the given PID exists
    ///
    /// This uses `kill(pid, 0)` which is a standard way to check if a process exists
    /// without actually sending a signal. It returns true if the process exists.
    fn process_exists(pid: u32) -> bool
    {
        // Use std::process::Command to check if process exists
        // This is safer than using libc directly
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Attach to a process and read its registers using ferros-core
    ///
    /// This function demonstrates how to use the ferros-core library:
    /// 1. Create a debugger instance
    /// 2. Attach to the target process
    /// 3. Read registers
    /// 4. Display them
    ///
    /// All the low-level Mach API calls are hidden inside ferros-core,
    /// making this code much simpler and easier to understand.
    fn attach_and_read_registers(pid: u32) -> Result<(), ferros_core::error::DebuggerError>
    {
        // Step 1: Create a debugger instance
        // This doesn't attach to anything yet - it just creates an empty debugger
        let mut debugger = MacOSDebugger::new()?;

        // Step 2: Attach to the target process
        // This internally calls:
        //   - task_for_pid() to get a Mach task port
        //   - task_threads() to get the main thread
        // All error handling is done by ferros-core
        debugger.attach(ProcessId::from(pid))?;
        println!("✅ Successfully attached to process {pid}");

        // Step 3: Read registers from the attached process
        // This internally calls:
        //   - thread_get_state() with ARM_THREAD_STATE64 flavor
        //   - Parses the register layout
        //   - Returns a platform-agnostic Registers struct
        let regs = debugger.read_registers()?;
        println!("✅ Successfully read registers\n");

        // Step 4: Display the register values
        // The Registers struct provides a clean, platform-agnostic interface
        // We don't need to know about ARM64 vs x86-64 here!
        display_registers(&regs);

        // Step 5: Detach (optional on macOS, but good practice)
        // On macOS, this just clears the internal state - the Mach ports
        // are automatically released when the debugger is dropped
        debugger.detach()?;

        Ok(())
    }

    /// Display register values in a readable format
    ///
    /// This function shows how to access register values from the Registers struct.
    /// The struct is platform-agnostic, so this code works on ARM64 and x86-64.
    fn display_registers(regs: &ferros_core::types::Registers)
    {
        println!("📊 Register Values:");
        println!("===================");

        // Special registers (same on all architectures)
        println!("PC (Program Counter):      {}", regs.pc);
        println!("SP (Stack Pointer):       {}", regs.sp);
        println!("FP (Frame Pointer):      {}", regs.fp);
        println!("Status Register:          0x{:016x}", regs.status);
        println!();

        // General-purpose registers
        // On ARM64, these are X0-X30
        // On x86-64, these would be RAX, RBX, RCX, RDX, RSI, RDI, R8-R15
        println!("General-Purpose Registers:");
        for (i, value) in regs.general.iter().enumerate() {
            // Use architecture-appropriate register names
            #[cfg(target_arch = "aarch64")]
            println!("  X{:2}:                      0x{:016x}", i, value);

            #[cfg(target_arch = "x86_64")]
            {
                let names = [
                    "RAX", "RBX", "RCX", "RDX", "RSI", "RDI", "R8", "R9", "R10", "R11", "R12", "R13", "R14", "R15",
                ];
                if i < names.len() {
                    println!("  {}:                      0x{:016x}", names[i], value);
                } else {
                    println!("  R{}:                      0x{:016x}", i, value);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main()
{
    macos_impl::main();
}
