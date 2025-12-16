use std::str::FromStr;

use clap::Parser;
use ferros_config::{Config, ConfigSource};
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

    // Load configuration (may create default global config on first run)
    let (config, source) =
        Config::load().map_err(|e| FerrosError::InvalidArgument(format!("Failed to load config: {e}")))?;

    // Initialize logging based on configuration and CLI options
    init_logging_from_cli(&cli, &config, source)
        .map_err(|e| FerrosError::InvalidArgument(format!("Failed to initialize logging: {}", e)))?;

    // Resolve the effective command, supporting both explicit subcommands
    // and configuration-driven defaults for bare invocations.
    let command = resolve_command(cli.command, &config)?;

    // Run the appropriate command based on the resolved input
    run_command(command)?;

    Ok(())
}

/// Initialize logging based on configuration and CLI options
///
/// Priority order:
/// 1. CLI `--log-level` and `--log-format` (if provided, override everything)
/// 2. CLI `--log-format` only (uses RUST_LOG env var or INFO default for level)
/// 3. Environment variables (RUST_LOG, FERROS_LOG_FORMAT)
///
/// If `--log-level` or `--log-format` are provided, they override environment variables.
/// Otherwise, falls back to `init_logging()` which reads from environment variables.
fn init_logging_from_cli(cli: &Cli, config: &Config, _source: ConfigSource) -> Result<(), LoggingError>
{
    // Parse log level in priority order:
    // 1. CLI flag
    // 2. Config file
    // 3. Environment (handled by init_logging)
    let cli_log_level = cli.log_level.as_deref().and_then(|s| LogLevel::from_str(s).ok());

    let config_log_level = match config.debugger.log_level {
        ferros_config::LogLevel::Error => Some(LogLevel::Error),
        ferros_config::LogLevel::Warn => Some(LogLevel::Warn),
        ferros_config::LogLevel::Info => Some(LogLevel::Info),
        ferros_config::LogLevel::Debug => Some(LogLevel::Debug),
        ferros_config::LogLevel::Trace => Some(LogLevel::Trace),
    };

    let log_level = cli_log_level.or(config_log_level);

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

/// Resolve the effective CLI command, supporting:
///
/// - Explicit subcommands: `attach`, `launch`
/// - Bare invocation using configuration fallback:
///   - With `debugger.default_attach_mode = "launch"`:
///     `ferros ./target/debug/my_program arg1 arg2`
///   - With `debugger.default_attach_mode = "attach"`:
///     `ferros 12345`
fn resolve_command(command: crate::cli::Commands, config: &Config) -> FerrosResult<crate::cli::Commands>
{
    use crate::cli::Commands;

    match command {
        // Explicit subcommands are passed through unchanged.
        Commands::Attach { .. } | Commands::Launch { .. } => Ok(command),

        // External subcommands are interpreted according to configuration.
        Commands::External(args) => {
            if args.is_empty() {
                return Err(FerrosError::InvalidArgument(
                    "No target program or PID provided. Specify a subcommand or a target.".to_string(),
                ));
            }

            let mode = config
                .debugger
                .default_attach_mode
                .as_deref()
                .unwrap_or("launch")
                .to_lowercase();

            match mode.as_str() {
                "launch" => {
                    let program = args[0].clone();
                    let launch_args = if args.len() > 1 { args[1..].to_vec() } else { Vec::new() };

                    Ok(Commands::Launch {
                        program,
                        args: launch_args,
                    })
                }
                "attach" => {
                    let pid_str = &args[0];
                    let pid = pid_str.parse::<u32>().map_err(|_| {
                        FerrosError::InvalidArgument(format!(
                            "Invalid PID '{pid_str}' for attach mode. Expected a positive integer."
                        ))
                    })?;

                    Ok(Commands::Attach { pid })
                }
                other => Err(FerrosError::InvalidArgument(format!(
                    "Invalid debugger.default_attach_mode '{other}'. Expected 'launch' or 'attach'."
                ))),
            }
        }
    }
}
