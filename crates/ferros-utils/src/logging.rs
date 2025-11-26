//! # Logging Utilities
//!
//! Logging infrastructure for Ferros using `tracing`.
//!
//! This module provides structured logging with support for:
//! - Multiple output formats (JSON for production, pretty for development)
//! - Environment variable configuration
//! - Log level filtering
//! - File and console output
//! - Structured fields and spans
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use ferros_utils::init_logging;
//!
//! // Initialize with default settings (reads from RUST_LOG env var)
//! init_logging().expect("Failed to initialize logging");
//!
//! // Use tracing macros throughout your code
//! tracing::info!("Application started");
//! tracing::debug!("Debug information");
//! tracing::error!("An error occurred");
//! ```
//!
//! ## Environment Variables
//!
//! - `RUST_LOG`: Set log level filter (e.g., `RUST_LOG=debug`, `RUST_LOG=ferros_core=debug`)
//! - `FERROS_LOG_FORMAT`: Set output format (`json` or `pretty`, default: `pretty`)
//! - `FERROS_LOG_FILE`: Optional path to log file (if not set, logs only to console)
//!
//! ## Examples
//!
//! ```rust,no_run
//! use ferros_utils::{LogFormat, LogLevel, init_logging_with_level};
//!
//! // Initialize with specific log level
//! init_logging_with_level(LogLevel::Debug, LogFormat::Pretty)
//!     .expect("Failed to initialize logging");
//!
//! // Initialize with JSON output for production
//! init_logging_with_level(LogLevel::Info, LogFormat::Json).expect("Failed to initialize logging");
//! ```

use std::path::PathBuf;
use std::str::FromStr;
use std::{env, io};

use chrono::Utc;
use tracing::Level;
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::fmt::{self};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// Log output format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat
{
    /// Pretty-printed, human-readable format (default for development)
    Pretty,
    /// JSON format (default for production)
    Json,
}

impl FromStr for LogFormat
{
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err>
    {
        match s.to_lowercase().as_str() {
            "pretty" | "dev" | "development" => Ok(LogFormat::Pretty),
            "json" | "prod" | "production" => Ok(LogFormat::Json),
            _ => Err(format!("Unknown log format: {s}. Use 'pretty' or 'json'")),
        }
    }
}

/// Log level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel
{
    /// Error level
    Error,
    /// Warning level
    Warn,
    /// Info level (default)
    Info,
    /// Debug level
    Debug,
    /// Trace level (most verbose)
    Trace,
}

impl From<LogLevel> for Level
{
    fn from(level: LogLevel) -> Self
    {
        match level {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Trace => Level::TRACE,
        }
    }
}

impl FromStr for LogLevel
{
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err>
    {
        match s.to_lowercase().as_str() {
            "error" | "err" => Ok(LogLevel::Error),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" | "dbg" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(format!(
                "Unknown log level: {s}. Use 'error', 'warn', 'info', 'debug', or 'trace'"
            )),
        }
    }
}

/// Initialize logging with default settings
///
/// Reads configuration from environment variables:
/// - `RUST_LOG`: Log level filter (e.g., `debug`, `ferros_core=debug`)
/// - `FERROS_LOG_FORMAT`: Output format (`json` or `pretty`, default: `pretty`)
/// - `FERROS_LOG_FILE`: Optional path to log file
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_utils::init_logging;
///
/// init_logging().expect("Failed to initialize logging");
/// tracing::info!("Application started");
/// ```
///
/// ## Errors
///
/// Returns an error if:
/// - Logging is already initialized
/// - Invalid environment variable values
/// - File logging fails (if `FERROS_LOG_FILE` is set)
pub fn init_logging() -> Result<(), LoggingError>
{
    // Read format from environment or default to pretty
    let format = env::var("FERROS_LOG_FORMAT")
        .ok()
        .and_then(|s| LogFormat::from_str(&s).ok())
        .unwrap_or(LogFormat::Pretty);

    // Read log level from RUST_LOG or default to INFO
    let default_level = env::var("RUST_LOG")
        .unwrap_or_else(|_| "info".to_string())
        .parse::<LogLevel>()
        .map(Into::into)
        .unwrap_or(Level::INFO);

    init_logging_internal(format, default_level)
}

/// Initialize logging with explicit level and format
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_utils::{LogFormat, LogLevel, init_logging_with_level};
///
/// init_logging_with_level(LogLevel::Debug, LogFormat::Pretty)
///     .expect("Failed to initialize logging");
/// ```
///
/// ## Errors
///
/// Returns an error if logging is already initialized or file logging fails.
pub fn init_logging_with_level(level: LogLevel, format: LogFormat) -> Result<(), LoggingError>
{
    init_logging_internal(format, level.into())
}

/// Initialize logging for TUI mode (file-only, no stdout)
///
/// This function configures logging to write only to a file, not to stdout/stderr,
/// which prevents log messages from interfering with the TUI display.
///
/// The log file will be created in the user's home directory at `~/.ferros/YYYY-MM-DD-ferros-tui.log`
/// or falls back to `/tmp/YYYY-MM-DD-ferros-tui.log` if home directory is not accessible.
///
/// ## Arguments
///
/// * `level` - Optional log level. If `None`, uses `RUST_LOG` environment variable or defaults to `INFO`.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_utils::{LogLevel, init_logging_for_tui};
///
/// // Use default (INFO or RUST_LOG)
/// init_logging_for_tui(None).expect("Failed to initialize logging for TUI");
///
/// // Or specify a level explicitly
/// init_logging_for_tui(Some(LogLevel::Debug)).expect("Failed to initialize logging for TUI");
/// ```
///
/// ## Errors
///
/// Returns an error if logging is already initialized or file creation fails.
pub fn init_logging_for_tui(level: Option<LogLevel>) -> Result<PathBuf, LoggingError>
{
    // Determine log file path with date prefix
    let today = Utc::now().format("%Y-%m-%d");
    let log_file = if let Ok(home) = env::var("HOME") {
        let ferros_dir = PathBuf::from(home).join(".ferros");
        // Create directory if it doesn't exist
        std::fs::create_dir_all(&ferros_dir).map_err(LoggingError::FileError)?;
        ferros_dir.join(format!("{today}-ferros-tui.log"))
    } else {
        PathBuf::from("/tmp").join(format!("{today}-ferros-tui.log"))
    };

    let explicit_level = level.map(Into::into);
    init_logging_file_only(log_file.clone(), LogFormat::Pretty, explicit_level)?;
    Ok(log_file)
}

/// Internal initialization function
#[allow(clippy::unnecessary_wraps)]
fn init_logging_internal(format: LogFormat, default_level: Level) -> Result<(), LoggingError>
{
    // Build environment filter
    // RUST_LOG can override the default level with more specific filters
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level.to_string()));

    // Check if file logging is requested
    let log_file = env::var("FERROS_LOG_FILE").ok().map(PathBuf::from);

    match format {
        LogFormat::Pretty => {
            let console_layer = fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(ChronoUtc::rfc_3339())
                .with_ansi(true)
                .with_writer(io::stdout)
                .with_filter(env_filter.clone());

            if let Some(file_path) = log_file {
                // File logging with pretty format
                let file_appender = tracing_appender::rolling::daily(
                    file_path.parent().unwrap_or(&PathBuf::from(".")),
                    file_path.file_name().unwrap_or_default(),
                );
                let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
                let file_layer = fmt::layer()
                    .with_writer(non_blocking)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_timer(ChronoUtc::rfc_3339())
                    .with_ansi(false) // No ANSI in files
                    .with_filter(env_filter);

                Registry::default().with(console_layer).with(file_layer).init();
            } else {
                // Console only
                Registry::default().with(console_layer).init();
            }
        }
        LogFormat::Json => {
            let console_layer = fmt::layer()
                .json()
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(ChronoUtc::rfc_3339())
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(io::stdout)
                .with_filter(env_filter.clone());

            if let Some(file_path) = log_file {
                // File logging with JSON format
                let file_appender = tracing_appender::rolling::daily(
                    file_path.parent().unwrap_or(&PathBuf::from(".")),
                    file_path.file_name().unwrap_or_default(),
                );
                let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
                let file_layer = fmt::layer()
                    .json()
                    .with_writer(non_blocking)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_timer(ChronoUtc::rfc_3339())
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_filter(env_filter);

                Registry::default().with(console_layer).with(file_layer).init();
            } else {
                // Console only
                Registry::default().with(console_layer).init();
            }
        }
    }

    Ok(())
}

/// Internal initialization function for file-only logging
/// Used by TUI mode to prevent stdout interference
#[allow(clippy::unnecessary_wraps)]
fn init_logging_file_only(log_file: PathBuf, format: LogFormat, explicit_level: Option<Level>) -> Result<(), LoggingError>
{
    // Build environment filter
    // Priority:
    // 1. If explicit_level is Some (from --log-level CLI flag), use it
    // 2. If RUST_LOG is set, use it (allows module-specific filters like "ferros_core=debug")
    // 3. Otherwise, use INFO as default
    let env_filter = if let Some(level) = explicit_level {
        // Explicit level from CLI takes precedence
        EnvFilter::new(level.to_string())
    } else if let Ok(rust_log) = env::var("RUST_LOG") {
        // Use RUST_LOG (supports both simple levels and module-specific filters)
        EnvFilter::try_new(&rust_log).unwrap_or_else(|_| EnvFilter::new(Level::INFO.to_string()))
    } else {
        // Default to INFO
        EnvFilter::new(Level::INFO.to_string())
    };

    match format {
        LogFormat::Pretty => {
            // File logging only with pretty format
            // Use rolling::never() since we're already including the date in the filename
            let file_appender = tracing_appender::rolling::never(
                log_file.parent().unwrap_or(&PathBuf::from(".")),
                log_file.file_name().unwrap_or_default(),
            );
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            // Store the guard to prevent it from being dropped
            std::mem::forget(_guard);

            let file_layer = fmt::layer()
                .with_writer(non_blocking)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(ChronoUtc::rfc_3339())
                .with_ansi(false) // No ANSI in files
                .with_filter(env_filter);

            Registry::default().with(file_layer).init();
        }
        LogFormat::Json => {
            // File logging only with JSON format
            // Use rolling::never() since we're already including the date in the filename
            let file_appender = tracing_appender::rolling::never(
                log_file.parent().unwrap_or(&PathBuf::from(".")),
                log_file.file_name().unwrap_or_default(),
            );
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            // Store the guard to prevent it from being dropped
            std::mem::forget(_guard);

            let file_layer = fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(ChronoUtc::rfc_3339())
                .with_current_span(true)
                .with_span_list(true)
                .with_filter(env_filter);

            Registry::default().with(file_layer).init();
        }
    }

    Ok(())
}

/// Logging initialization error
#[derive(Debug, thiserror::Error)]
pub enum LoggingError
{
    /// Invalid log format
    #[error("Invalid log format: {0}")]
    InvalidFormat(String),

    /// Invalid log level
    #[error("Invalid log level: {0}")]
    InvalidLevel(String),

    /// Failed to initialize logging
    #[error("Failed to initialize logging: {0}")]
    InitializationFailed(String),

    /// File logging error
    #[error("File logging error: {0}")]
    FileError(#[from] io::Error),
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn test_log_format_from_str()
    {
        assert_eq!(LogFormat::from_str("pretty").unwrap(), LogFormat::Pretty);
        assert_eq!(LogFormat::from_str("json").unwrap(), LogFormat::Json);
        assert_eq!(LogFormat::from_str("dev").unwrap(), LogFormat::Pretty);
        assert_eq!(LogFormat::from_str("prod").unwrap(), LogFormat::Json);
        assert!(LogFormat::from_str("invalid").is_err());
    }

    #[test]
    fn test_log_level_from_str()
    {
        assert_eq!(LogLevel::from_str("error").unwrap(), LogLevel::Error);
        assert_eq!(LogLevel::from_str("warn").unwrap(), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("info").unwrap(), LogLevel::Info);
        assert_eq!(LogLevel::from_str("debug").unwrap(), LogLevel::Debug);
        assert_eq!(LogLevel::from_str("trace").unwrap(), LogLevel::Trace);
        assert!(LogLevel::from_str("invalid").is_err());
    }

    #[test]
    fn test_log_level_to_tracing_level()
    {
        assert_eq!(Level::from(LogLevel::Error), Level::ERROR);
        assert_eq!(Level::from(LogLevel::Warn), Level::WARN);
        assert_eq!(Level::from(LogLevel::Info), Level::INFO);
        assert_eq!(Level::from(LogLevel::Debug), Level::DEBUG);
        assert_eq!(Level::from(LogLevel::Trace), Level::TRACE);
    }
}
