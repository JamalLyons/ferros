use clap::{Parser, Subcommand};

/// A Rust-native debugger with hybrid MIR and system-level introspection.
#[derive(Parser, Debug)]
#[command(name = "ferros")]
#[command(version)]
#[command(about = "A rust debugger", long_about = None)]
pub struct Cli
{
    /// Set the log level (error, warn, info, debug, trace)
    /// Overrides RUST_LOG environment variable
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// Set the log format (pretty, json)
    /// Overrides FERROS_LOG_FORMAT environment variable
    #[arg(long, value_name = "FORMAT")]
    pub log_format: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands
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
        /// Note: To set Ferros log level, use --log-level before the 'launch' subcommand:
        ///   ferros --log-level debug launch <program>
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}
