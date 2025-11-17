use std::process;

use clap::{Parser, Subcommand};
use ferros_core::debugger::create_debugger;
use ferros_core::types::ProcessId;
use ferros_core::{Debugger, Result as DebuggerResult};
use ferros_utils::{info, init_logging};

/// A Rust-native debugger with hybrid MIR and system-level introspection.
#[derive(Parser, Debug)]
#[command(name = "ferros")]
#[command(version)]
#[command(about = "A Rust-native debugger with hybrid MIR and system-level introspection", long_about = None)]
struct Cli
{
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands
{
    /// Attach to a running process by PID
    Attach
    {
        /// Process ID (PID) to attach to
        pid: u32,
    },
    /// Launch a new process under debugger control
    Launch
    {
        /// Path to the executable to launch
        program: String,
        /// Arguments to pass to the program
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Display CPU registers from the attached process
    Registers,
    /// Read memory from the attached process
    Memory
    {
        /// Memory address to read from (hex format: 0x1000 or decimal)
        address: String,
        /// Number of bytes to read (default: 16)
        #[arg(short, long, default_value_t = 16)]
        length: usize,
    },
    /// List memory regions in the attached process
    Regions,
    /// List all threads in the attached process
    Threads,
    /// Suspend execution of the attached process
    Suspend,
    /// Resume execution of the attached process
    Resume,
    /// Detach from the attached process
    Detach,
    /// Show debugger information (architecture, status, etc.)
    Info,
}

fn main()
{
    // Initialize logging (reads from RUST_LOG env var)
    // Defaults to INFO level and Pretty format if not set
    if let Err(e) = init_logging() {
        eprintln!("Failed to initialize logging: {}", e);
        process::exit(1);
    }

    let cli = Cli::parse();

    if let Err(e) = run_command(cli) {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}

fn run_command(cli: Cli) -> DebuggerResult<()>
{
    match cli.command {
        Commands::Attach { pid } => {
            info!("Attaching to process {}", pid);
            let mut debugger = create_debugger()?;
            debugger.attach(ProcessId::from(pid))?;
            println!("Successfully attached to process {}", pid);
            print_debugger_info(&*debugger)?;
            Ok(())
        }
        Commands::Launch { program, args } => {
            info!("Launching program: {} with args: {:?}", program, args);
            let mut debugger = create_debugger()?;
            let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            debugger.launch(&program, &args_refs)?;
            println!("Successfully launched program: {}", program);
            print_debugger_info(&*debugger)?;
            Ok(())
        }
        Commands::Registers => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Memory { address: _, length: _ } => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Regions => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Threads => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Suspend => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Resume => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Detach => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
        Commands::Info => {
            // TODO: Implement state management to persist debugger instance
            eprintln!("Error: No process attached. Use 'ferros attach <pid>' or 'ferros launch <program>' first.");
            eprintln!(
                "Note: This command requires an attached process. State management will be added in a future version."
            );
            Err(ferros_core::error::DebuggerError::NotAttached)
        }
    }
}

fn print_debugger_info(debugger: &dyn Debugger) -> DebuggerResult<()>
{
    println!("\nDebugger Information:");
    println!("  Architecture: {}", debugger.architecture());
    println!("  Attached: {}", debugger.is_attached());
    println!("  Stopped: {}", debugger.is_stopped());
    println!("  Stop Reason: {:?}", debugger.stop_reason());

    if debugger.is_attached() {
        if let Ok(threads) = debugger.threads() {
            println!("  Threads: {}", threads.len());
            if let Some(active) = debugger.active_thread() {
                println!("  Active Thread: {}", active.raw());
            }
        }

        if let Ok(regions) = debugger.get_memory_regions() {
            println!("  Memory Regions: {}", regions.len());
        }
    }

    Ok(())
}
