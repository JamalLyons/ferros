use clap::Parser;

use crate::commands::Commands;

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
