use ferros_core::prelude::*;

use crate::cli::Commands;
use crate::repl::Repl;

/// Run a command based on CLI input
pub fn run_command(command: Commands) -> FerrosResult<()>
{
    match command {
        Commands::Attach { pid } => {
            let mut debugger = create_debugger()?;
            debugger.attach(ProcessId::from(pid))?;

            // Enter REPL mode
            let mut repl = Repl::new(debugger, pid, false)?;
            repl.run()?;

            Ok(())
        }
        Commands::Launch { program, args } => {
            let mut debugger = create_debugger()?;
            // Convert Vec<String> to Vec<&str>
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let pid = debugger.launch(&program, &arg_refs)?;

            // Enter REPL; since we launched, we should terminate the target when we exit
            let mut repl = Repl::new(debugger, pid.into(), true)?;
            repl.run()?;

            Ok(())
        }
        // `External` should be resolved into a concrete `Attach` or `Launch`
        // command by `resolve_command` in `main.rs` and never reach here.
        // If it does, treat it as a programmer error and surface a clear message.
        Commands::External(args) => Err(FerrosError::InvalidArgument(format!(
            "Unresolved external command '{:?}'. This is a bug in ferros; please file an issue including the CLI \
             invocation.",
            args
        ))),
    }
}

/// Execute a REPL command
///
/// This function handles all the commands that can be executed within the REPL.
/// Returns whether the REPL should continue running.
pub fn execute_repl_command(debugger: &mut Box<dyn FerrosDebugger>, pid: u32, input: &str) -> FerrosResult<bool>
{
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(true);
    }

    let command = parts[0];
    let _args = &parts[1..];

    match command {
        "help" | "h" => {
            print_help();
            Ok(true)
        }
        "quit" | "q" | "exit" => {
            debugger.detach()?;
            Ok(false)
        }
        "detach" => {
            println!("Detaching from process {}...", pid);
            debugger.detach()?;
            println!("Detached successfully.");
            Ok(false)
        }
        "info" | "i" => {
            print_info(pid);
            Ok(true)
        }
        "threads" | "t" => {
            print_threads();
            Ok(true)
        }
        _ => {
            eprintln!("Unknown command: {}. Type 'help' for available commands.", command);
            Ok(true)
        }
    }
}

/// Print help information
fn print_help()
{
    println!("Available commands:");
    println!("  help, h              - Show this help message");
    println!("  quit, q, exit        - Detach and exit the REPL");
    println!("  detach               - Detach from the process and exit");
    println!("  info, i              - Show information about the attached process");
    println!("  threads, t           - List threads in the attached process");
}

/// Print information about the attached process
fn print_info(pid: u32)
{
    println!("Process ID: {}", pid);
    // TODO: Add more info as debugger methods become available
    // - Architecture
    // - Thread count
    // - Memory regions
    // - etc.
}

/// Print thread information
fn print_threads()
{
    // TODO: Implement when thread enumeration is available in the debugger trait
    println!("Thread listing not yet implemented.");
    println!("(This will show all threads when thread enumeration is added to the debugger trait)");
}
