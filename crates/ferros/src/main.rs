mod platform;

use std::process;

use clap::{Parser, Subcommand};
use ferros_core::types::ProcessId;
use ferros_core::{LaunchSpec, Result as DebuggerResult, Session};
use ferros_utils::{LogFormat, LogLevel, debug, info, init_logging, init_logging_with_level};

use crate::platform::create_backend;

/// A Rust-native debugger with hybrid MIR and system-level introspection.
#[derive(Parser, Debug)]
#[command(name = "ferros")]
#[command(version)]
#[command(about = "A Rust-native debugger with hybrid MIR and system-level introspection", long_about = None)]
struct Cli
{
    /// Set the log level (error, warn, info, debug, trace)
    /// Overrides `RUST_LOG` environment variable
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Set the log format (pretty, json)
    /// Overrides `FERROS_LOG_FORMAT` environment variable
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
        /// Use headless mode (print info and exit; currently the only mode)
        #[arg(long, default_value_t = false)]
        headless: bool,
    },
    /// Launch a new process under debugger control
    Launch
    {
        /// Path to the executable to launch
        program: String,
        /// Arguments to pass to the program
        /// Note: To set Ferros log level, use --log-level before the 'launch' subcommand:
        ///   ferros --log-level debug launch <program>
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Use headless mode (print info and exit; currently the only mode)
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
    let mut cli = Cli::parse();

    // The interactive front end is being rebuilt; non-headless behaves like --headless.
    if let Commands::Attach { headless, .. } | Commands::Launch { headless, .. } = &mut cli.command
        && !*headless
    {
        println!("The interactive front end is being rebuilt; running in headless mode.");
        *headless = true;
    }

    // Initialize logging with CLI flags or environment variables
    let _log_file_path: Option<std::path::PathBuf> = if let Some(level_str) = &cli.log_level {
        // Parse log level from CLI
        let level = level_str.parse::<LogLevel>().unwrap_or_else(|_| {
            eprintln!("Invalid log level: {level_str}. Use: error, warn, info, debug, or trace");
            process::exit(1);
        });

        // Parse log format from CLI or default to pretty
        let format = cli
            .log_format
            .as_ref()
            .and_then(|f| f.parse::<LogFormat>().ok())
            .unwrap_or(LogFormat::Pretty);

        if let Err(e) = init_logging_with_level(level, format) {
            eprintln!("Failed to initialize logging: {e}");
            process::exit(1);
        }
        None
    } else {
        // Use environment variables or defaults
        if let Err(e) = init_logging() {
            eprintln!("Failed to initialize logging: {e}");
            process::exit(1);
        }
        None
    };

    // Handle find-logs command early
    if matches!(cli.command, Commands::FindLogs) {
        let log_dir = if let Ok(home) = std::env::var("HOME") {
            std::path::PathBuf::from(home).join(".ferros")
        } else {
            std::path::PathBuf::from("/tmp")
        };

        println!("{}", log_dir.display());
        return;
    }

    if let Err(e) = run_command(cli) {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn run_command(cli: Cli) -> DebuggerResult<()>
{
    match cli.command {
        Commands::Attach { pid, headless: true } => {
            info!("Attaching to process {}", pid);
            let mut session = Session::new(create_backend()?);
            session.attach(ProcessId::from(pid))?;
            info!("Successfully attached to process {}", pid);
            print_debugger_info(&mut session);
            // Detach after showing info in headless mode; the process keeps running.
            session.detach()?;
            Ok(())
        }
        Commands::Launch {
            program,
            args,
            headless: true,
        } => {
            info!("Launching program: {} with args: {:?}", program, args);
            let mut session = Session::new(create_backend()?);

            // Convert relative path to absolute path for posix_spawn
            let program_path = std::path::Path::new(&program);
            let absolute_program = if program_path.is_absolute() {
                program_path.to_path_buf()
            } else {
                std::env::current_dir()?.join(program_path).canonicalize()?
            };

            // `args` are the program's arguments; the backend passes the program as argv[0].
            let spec = LaunchSpec::new(&absolute_program).args(args);
            let pid = session.launch(&spec)?;
            info!(
                "Successfully launched program: {} (PID: {})",
                absolute_program.display(),
                pid.0
            );

            // The process is stopped at entry: show the info, then detach so it runs.
            print_debugger_info(&mut session);
            session.detach()?;
            info!("Process detached and running");
            Ok(())
        }
        Commands::Attach { headless: false, .. } | Commands::Launch { headless: false, .. } => {
            unreachable!("non-headless is normalized to headless in main()")
        }
        Commands::Registers
        | Commands::Memory { .. }
        | Commands::Regions
        | Commands::Threads
        | Commands::Suspend
        | Commands::Resume
        | Commands::Detach
        | Commands::Info => {
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

fn print_debugger_info(session: &mut Session)
{
    info!("Debugger Information:");
    info!("  Architecture: {}", session.architecture());
    info!("  State: {:?}", session.state());
    if let Some(pid) = session.pid() {
        info!("  PID: {}", pid.0);
    }
    debug!("  Last Stop: {:?}", session.last_stop());

    if let Ok(threads) = session.threads() {
        info!("  Threads: {}", threads.len());
        if let Some(selected) = session.selected_thread() {
            debug!("  Selected Thread: {}", selected.raw());
        }
    }

    if let Ok(regions) = session.memory_regions() {
        info!("  Memory Regions: {}", regions.len());
    }
}
