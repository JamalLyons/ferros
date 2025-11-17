//! Memory Inspector Example
//!
//! Demonstrates Project 1.2: Reading and writing process memory
//!
//! ## What this example does:
//!
//! 1. Attaches to hello_target process
//! 2. Lists memory regions
//! 3. Reads the TEST_VARIABLE from memory (if address provided)
//! 4. Modifies the variable's value
//! 5. Verifies the change
//!
//! ## Usage:
//!
//! ```bash
//! # Terminal 1: Start target process
//! cargo run --example hello_target
//!
//! # Terminal 2: Attach debugger (note the PID and address from terminal 1)
//! sudo cargo run --example memory_inspector <PID> [variable_address]
//! ```

#[cfg(not(target_os = "macos"))]
fn main()
{
    eprintln!("This example is macOS-only.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod macos_impl
{
    use std::process;

    use ferros_core::platform::macos::MacOSDebugger;
    use ferros_core::types::{Address, ProcessId};
    use ferros_core::Debugger;

    pub fn main()
    {
        let args: Vec<String> = std::env::args().collect();

        if args.len() < 2 {
            eprintln!("Usage: {} <PID> [variable_address]", args[0]);
            eprintln!("Example: {} 12345 0x100000000", args[0]);
            eprintln!("\nFirst, run: cargo run --example hello_target");
            eprintln!("Then run with sudo: sudo cargo run --example memory_inspector <PID> [address]");
            process::exit(1);
        }

        let pid: u32 = match args[1].parse() {
            Ok(pid) => pid,
            Err(e) => {
                eprintln!("Error: Invalid PID '{}': {}", args[1], e);
                process::exit(1);
            }
        };

        let var_addr: Option<Address> = args.get(2).and_then(|s| {
            let value = if let Some(stripped) = s.strip_prefix("0x") {
                u64::from_str_radix(stripped, 16).ok()
            } else {
                s.parse().ok()
            }?;
            Some(Address::from(value))
        });

        println!("🔍 Ferros Memory Inspector");
        println!("==========================");
        println!("Platform: macOS");
        println!("Target PID: {pid}\n");

        match inspect_memory(pid, var_addr) {
            Ok(()) => {
                println!("\n✅ Memory inspection completed successfully");
            }
            Err(e) => {
                eprintln!("❌ Error: {}", e);
                process::exit(1);
            }
        }
    }

    fn inspect_memory(pid: u32, var_addr: Option<Address>) -> Result<(), ferros_core::error::DebuggerError>
    {
        let mut debugger = MacOSDebugger::new()?;
        debugger.attach(ProcessId::from(pid))?;
        println!("✅ Attached to process {pid}\n");

        // Task 1.2.4: Parse memory regions
        println!("📋 Memory Regions:");
        println!("===================");
        let regions = debugger.get_memory_regions()?;
        for (i, region) in regions.iter().take(10).enumerate() {
            println!(
                "{:2}. {}-{} {} ({} bytes) {:?}",
                i + 1,
                region.start,
                region.end,
                region.permissions,
                region.size(),
                region.name
            );
        }
        if regions.len() > 10 {
            println!("... and {} more regions", regions.len() - 10);
        }
        println!();

        // Task 1.2.2: Read memory
        if let Some(addr) = var_addr {
            println!("📖 Reading memory at {}:", addr);
            let data = debugger.read_memory(addr, 8)?; // Read 8 bytes (u64)
            println!("   Raw bytes: {:?}", data);

            if data.len() >= 8 {
                let value = u64::from_le_bytes([data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7]]);
                println!("   As u64 (LE): 0x{:016x}", value);
                println!("   Expected:    0xDEADBEEFCAFEBABE");

                if value == 0xDEADBEEFCAFEBABE {
                    println!("   ✅ Value matches expected!");
                } else {
                    println!("   ⚠️  Value doesn't match (may have been modified)");
                }
            }
            println!();

            // Task 1.2.3: Write memory
            println!("✏️  Writing new value to memory:");
            let new_value: u64 = 0xCAFEBABEDEADBEEF;
            let new_bytes = new_value.to_le_bytes();
            debugger.write_memory(addr, &new_bytes)?;
            println!("   Wrote 0x{:016x} to address {}", new_value, addr);

            // Verify the write
            let verify_data = debugger.read_memory(addr, 8)?;
            if verify_data.len() >= 8 {
                let verify_value = u64::from_le_bytes([
                    verify_data[0],
                    verify_data[1],
                    verify_data[2],
                    verify_data[3],
                    verify_data[4],
                    verify_data[5],
                    verify_data[6],
                    verify_data[7],
                ]);
                println!("   Verified: 0x{:016x}", verify_value);
                if verify_value == new_value {
                    println!("   ✅ Write successful!");
                } else {
                    println!("   ❌ Write verification failed!");
                }
            }
        } else {
            println!("💡 To read/write memory, provide the variable address:");
            println!("   sudo cargo run --example memory_inspector {} <address>", pid);
            println!("   (Get the address from hello_target output)");
        }

        debugger.detach()?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn main()
{
    macos_impl::main();
}
