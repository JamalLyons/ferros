use std::process;

use clap::{Parser, Subcommand};
use ferros_core::debugger::create_debugger;
use ferros_core::types::ProcessId;
use ferros_core::{Debugger, Result as DebuggerResult};
use ferros_utils::{debug, info, init_logging, init_logging_for_tui, init_logging_with_level, LogFormat, LogLevel};

/// A Rust-native debugger with hybrid MIR and system-level introspection.
#[derive(Parser, Debug)]
#[command(name = "ferros")]
#[command(version)]
#[command(about = "A Rust-native debugger with hybrid MIR and system-level introspection", long_about = None)]
struct Cli
{
    /// Set the log level (error, warn, info, debug, trace)
    /// Overrides RUST_LOG environment variable
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Set the log format (pretty, json)
    /// Overrides FERROS_LOG_FORMAT environment variable
    #[arg(long, value_name = "FORMAT")]
    log_format: Option<String>,

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
        /// Use headless mode (no TUI, just print info and exit)
        #[arg(long, default_value_t = false)]
        headless: bool,
    },
    /// Launch a new process under debugger control
    Launch
    {
        /// Path to the executable to launch
        program: String,
        /// Arguments to pass to the program
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Use headless mode (no TUI, just print info and exit)
        #[arg(long, default_value_t = false)]
        headless: bool,
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
    /// Change directory to the log directory for easy log viewing
    FindLogs,
}

fn main()
{
    let cli = Cli::parse();

    // Check if we're running in TUI mode (non-headless attach/launch)
    let is_tui_mode = matches!(
        cli.command,
        Commands::Attach { headless: false, .. } | Commands::Launch { headless: false, .. }
    );

    // Initialize logging with CLI flags or environment variables
    let _log_file_path = if is_tui_mode {
        // For TUI mode, use file-only logging to prevent stdout interference
        match init_logging_for_tui() {
            Ok(path) => {
                // Log once to indicate where logs are being written
                // Note: This will go to the log file since we're in TUI mode
                info!("Logs are being written to: {}", path.display());
                Some(path)
            }
            Err(e) => {
                eprintln!("Failed to initialize logging: {}", e);
                process::exit(1);
            }
        }
    } else if let Some(level_str) = &cli.log_level {
        // Parse log level from CLI
        let level = level_str.parse::<LogLevel>().unwrap_or_else(|_| {
            eprintln!("Invalid log level: {}. Use: error, warn, info, debug, or trace", level_str);
            process::exit(1);
        });

        // Parse log format from CLI or default to pretty
        let format = cli
            .log_format
            .as_ref()
            .and_then(|f| f.parse::<LogFormat>().ok())
            .unwrap_or(LogFormat::Pretty);

        if let Err(e) = init_logging_with_level(level, format) {
            eprintln!("Failed to initialize logging: {}", e);
            process::exit(1);
        }
        None
    } else {
        // Use environment variables or defaults
        if let Err(e) = init_logging() {
            eprintln!("Failed to initialize logging: {}", e);
            process::exit(1);
        }
        None
    };

    // Check if we need async runtime for TUI (default mode, unless --headless is used)
    let needs_async = matches!(
        cli.command,
        Commands::Attach { headless: false, .. } | Commands::Launch { headless: false, .. }
    );

    // Handle find-logs command early (before async runtime)
    if matches!(cli.command, Commands::FindLogs) {
        let log_dir = if let Ok(home) = std::env::var("HOME") {
            std::path::PathBuf::from(home).join(".ferros")
        } else {
            std::path::PathBuf::from("/tmp")
        };

        println!("{}", log_dir.display());
        return;
    }

    if needs_async {
        let rt = tokio::runtime::Runtime::new().unwrap();
        if let Err(e) = rt.block_on(run_command_async(cli)) {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    } else if let Err(e) = run_command(cli) {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}

async fn run_command_async(cli: Cli) -> Result<(), Box<dyn std::error::Error>>
{
    match cli.command {
        Commands::Attach { pid, headless } => {
            info!("Attaching to process {}", pid);
            let mut debugger = create_debugger()?;
            debugger.attach(ProcessId::from(pid))?;
            info!("Successfully attached to process {}", pid);

            if headless {
                print_debugger_info(&*debugger)?;
                // In headless mode, detach after showing info
                debugger.detach()?;
            } else {
                ferros_ui::run_tui(debugger, Some(pid), false).await?;
            }
            Ok(())
        }
        Commands::Launch { program, args, headless } => {
            info!("Launching program: {} with args: {:?}", program, args);
            let mut debugger = create_debugger()?;

            if !headless {
                debugger.set_capture_process_output(true);
            }

            // Convert relative path to absolute path for posix_spawn
            let program_path = std::path::Path::new(&program);
            let absolute_program = if program_path.is_absolute() {
                program.clone()
            } else {
                std::env::current_dir()?
                    .join(program_path)
                    .canonicalize()?
                    .to_string_lossy()
                    .to_string()
            };

            // The launch method requires at least one argument (typically the program name)
            // If no args provided, use the program name itself
            let args_refs: Vec<&str> = if args.is_empty() {
                vec![&absolute_program]
            } else {
                args.iter().map(|s| s.as_str()).collect()
            };

            let pid = debugger.launch(&absolute_program, &args_refs)?;

            // Process starts suspended, resume it so it runs normally
            debugger.resume()?;

            if headless {
                print_debugger_info(&*debugger)?;
                // In headless mode, detach after showing info
                debugger.detach()?;
            } else {
                ferros_ui::run_tui(debugger, Some(pid.0), true).await?;
            }
            Ok(())
        }
        _ => {
            // Non-async commands should not reach here
            Err("TUI mode only available for attach/launch commands".into())
        }
    }
}

fn run_command(cli: Cli) -> DebuggerResult<()>
{
    match cli.command {
        Commands::Attach { pid, headless: true } => {
            info!("Attaching to process {}", pid);
            let mut debugger = create_debugger()?;
            debugger.attach(ProcessId::from(pid))?;
            info!("Successfully attached to process {}", pid);
            print_debugger_info(&*debugger)?;
            // Detach after showing info in headless mode
            debugger.detach()?;
            Ok(())
        }
        Commands::Launch {
            program,
            args,
            headless: true,
        } => {
            info!("Launching program: {} with args: {:?}", program, args);
            let mut debugger = create_debugger()?;

            // Convert relative path to absolute path for posix_spawn
            let program_path = std::path::Path::new(&program);
            let absolute_program = if program_path.is_absolute() {
                program.clone()
            } else {
                std::env::current_dir()?
                    .join(program_path)
                    .canonicalize()?
                    .to_string_lossy()
                    .to_string()
            };

            // If no args provided, use the program name itself
            let args_refs: Vec<&str> = if args.is_empty() {
                vec![&absolute_program]
            } else {
                args.iter().map(|s| s.as_str()).collect()
            };

            let pid = debugger.launch(&absolute_program, &args_refs)?;
            info!("Successfully launched program: {} (PID: {})", absolute_program, pid.0);

            // Process starts suspended, resume it so it runs normally
            debugger.resume()?;
            info!("Process resumed and running");

            print_debugger_info(&*debugger)?;
            // Detach after showing info in headless mode
            debugger.detach()?;
            Ok(())
        }
        Commands::Attach { headless: false, .. } | Commands::Launch { headless: false, .. } => {
            // These should be handled by run_command_async
            Err(ferros_core::error::DebuggerError::InvalidArgument(
                "TUI mode requires async runtime".to_string(),
            ))
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
        Commands::FindLogs => {
            // This should be handled in main() before reaching here
            unreachable!("FindLogs should be handled in main()")
        }
    }
}

fn print_debugger_info(debugger: &dyn Debugger) -> DebuggerResult<()>
{
    info!("Debugger Information:");
    info!("  Architecture: {}", debugger.architecture());
    info!("  Attached: {}", debugger.is_attached());
    info!("  Stopped: {}", debugger.is_stopped());
    debug!("  Stop Reason: {:?}", debugger.stop_reason());

    if debugger.is_attached() {
        if let Ok(threads) = debugger.threads() {
            info!("  Threads: {}", threads.len());
            if let Some(active) = debugger.active_thread() {
                debug!("  Active Thread: {}", active.raw());
            }
        }

        if let Ok(regions) = debugger.get_memory_regions() {
            info!("  Memory Regions: {}", regions.len());
        }
    }

    Ok(())
}
