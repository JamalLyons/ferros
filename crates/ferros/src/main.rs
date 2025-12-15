use std::str::FromStr;

use clap::Parser;
use ferros_core::prelude::*;
use ferros_utils::{LogFormat, LogLevel, LoggingError, init_logging, init_logging_with_level};

use crate::cli::Cli;
use crate::commands::run_command;

mod cli;
mod commands;
mod repl;

fn main() -> FerrosResult<()>
{
    let cli = Cli::parse();

    // Initialize logging based on CLI options
    init_logging_from_cli(&cli).map_err(|e| FerrosError::InvalidArgument(format!("Failed to initialize logging: {}", e)))?;

    // Run the appropriate command based on the CLI input
    run_command(cli.command)?;

    Ok(())
}

/// Initialize logging based on CLI configuration
///
/// Priority order:
/// 1. CLI `--log-level` and `--log-format` (if provided, override everything)
/// 2. CLI `--log-format` only (uses RUST_LOG env var or INFO default for level)
/// 3. Environment variables (RUST_LOG, FERROS_LOG_FORMAT)
///
/// If `--log-level` or `--log-format` are provided, they override environment variables.
/// Otherwise, falls back to `init_logging()` which reads from environment variables.
fn init_logging_from_cli(cli: &Cli) -> Result<(), LoggingError>
{
    // Parse log level if provided
    let log_level = cli.log_level.as_deref().and_then(|s| LogLevel::from_str(s).ok());

    // Parse log format if provided, otherwise default to pretty
    let log_format = cli
        .log_format
        .as_deref()
        .and_then(|s| LogFormat::from_str(s).ok())
        .unwrap_or(LogFormat::Pretty);

    // If log level is explicitly provided, use it with the format
    if let Some(level) = log_level {
        init_logging_with_level(level, log_format)?;
    } else if cli.log_format.is_some() {
        // Format is explicitly set but level isn't - use INFO as default
        // (User can still use RUST_LOG env var for more granular control)
        init_logging_with_level(LogLevel::Info, log_format)?;
    } else {
        // No CLI overrides, use environment variables (RUST_LOG, FERROS_LOG_FORMAT)
        init_logging()?;
    }

    Ok(())
}
