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

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
#[cfg(any(test, feature = "test-log-write-delay"))]
use std::time::Duration;
use std::{env, io};

use chrono::Utc;
use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_log::AsLog as _;
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::fmt::{self};
use tracing_subscriber::layer::SubscriberExt;
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

/// The directory that holds the Ferros log files, always an absolute path.
///
/// `~/.ferros` when `HOME` is set and not empty, otherwise the OS temporary
/// directory. A relative result (a relative `HOME` or `TMPDIR`) is joined onto
/// the working directory, and an empty `TMPDIR` means `/tmp` on Unix. Every file
/// logger opens this directory, and `ferros find-logs` prints it, so the two
/// cannot disagree. The path is built from the OS string, so a home directory
/// that is not valid Unicode works.
#[must_use]
pub fn log_directory() -> PathBuf
{
    let directory = match env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(".ferros"),
        _ => env::temp_dir(),
    };
    absolute_directory(directory)
}

/// Where an empty temporary-directory setting points.
#[cfg(unix)]
const EMPTY_TEMP_DIR: &str = "/tmp";
#[cfg(not(unix))]
const EMPTY_TEMP_DIR: &str = ".";

/// `directory` as an absolute path. `std::env::temp_dir` returns an empty path
/// for an empty `TMPDIR`, which a file opener would read as the working directory.
fn absolute_directory(directory: PathBuf) -> PathBuf
{
    if directory.is_absolute() {
        return directory;
    }
    let directory = if directory.as_os_str().is_empty() {
        PathBuf::from(EMPTY_TEMP_DIR)
    } else {
        directory
    };
    std::path::absolute(&directory).unwrap_or(directory)
}

/// Create [`log_directory`] if needed and return it.
fn open_log_directory() -> Result<PathBuf, LoggingError>
{
    let directory = log_directory();
    std::fs::create_dir_all(&directory).map_err(LoggingError::FileError)?;
    Ok(directory)
}

/// `YYYY-MM-DD-<stem>.log` for today (UTC).
fn dated_file_name(stem: &str) -> String
{
    format!("{}-{stem}.log", Utc::now().format("%Y-%m-%d"))
}

/// Open `file` (appending, created if missing) with the given rotation.
fn open_appender(file: &Path, rotation: Rotation) -> Result<RollingFileAppender, LoggingError>
{
    let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
    RollingFileAppender::builder()
        .rotation(rotation)
        .filename_prefix(name)
        .build(file.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| LoggingError::FileError(io::Error::other(error)))
}

/// A running file logger that owns its flush worker.
///
/// Records are written by a background worker. Keep the session alive until the
/// program is about to exit; dropping it (or [`LoggingSession::finish`]) flushes
/// the records written so far. Without it, records written just before exit can
/// be lost.
///
/// The flush is bounded: dropping waits about 100 ms to queue the shutdown and
/// then up to about one second for the worker to drain. A worker still behind
/// after that (a sink that blocks, or a full queue) is abandoned and its
/// remaining records are lost, so exit never hangs on the log file.
#[derive(Debug)]
#[must_use = "dropping the session stops the log worker; hold it until the program exits"]
pub struct LoggingSession
{
    file: PathBuf,
    directory: PathBuf,
    worker: WorkerGuard,
}

impl LoggingSession
{
    /// The log file this session writes to.
    #[must_use]
    pub fn path(&self) -> &Path
    {
        &self.file
    }

    /// The directory the logger opened (what `ferros find-logs` prints).
    #[must_use]
    pub fn directory(&self) -> &Path
    {
        &self.directory
    }

    /// Flush the records written so far (within the bound above) and stop the worker.
    pub fn finish(self)
    {
        drop(self.worker);
    }
}

/// Start file-only logging for an interactive or batch debugging session.
///
/// Nothing is written to stdout or stderr, so the session output stays
/// deterministic. The log file is `YYYY-MM-DD-ferros.log` in [`log_directory`].
/// `level` overrides `RUST_LOG`, which otherwise applies, defaulting to `INFO`.
///
/// The returned [`LoggingSession`] owns the background worker. Hold it until
/// the program returns from `main`; dropping it flushes the records written
/// just before exit.
///
/// ## Errors
///
/// [`LoggingError::FileError`] when the log directory or file cannot be opened.
/// [`LoggingError::InitializationFailed`] when a global subscriber is already installed.
pub fn start_file_logging(level: Option<LogLevel>, format: LogFormat) -> Result<LoggingSession, LoggingError>
{
    let directory = open_log_directory()?;
    let file = directory.join(dated_file_name("ferros"));
    let appender = open_appender(&file, Rotation::NEVER)?;
    #[cfg(any(test, feature = "test-log-write-delay"))]
    let appender = SlowWrites {
        inner: appender,
        delay: test_write_delay(),
    };
    let (writer, worker) = tracing_appender::non_blocking(appender);
    install_file_only(writer, format, level.map(Into::into))?;
    Ok(LoggingSession { file, directory, worker })
}

/// Test-only: milliseconds the background worker waits before each write.
///
/// The exit tests set it so the last record is provably still queued when the
/// program returns from `main`; only a flush that waits for the worker gets that
/// record into the file.
#[cfg(any(test, feature = "test-log-write-delay"))]
const TEST_WRITE_DELAY: &str = "FERROS_TEST_LOG_WRITE_DELAY_MS";

#[cfg(any(test, feature = "test-log-write-delay"))]
fn test_write_delay() -> Option<Duration>
{
    let millis = env::var(TEST_WRITE_DELAY).ok()?.parse().ok()?;
    Some(Duration::from_millis(millis))
}

/// The log file behind the background worker, optionally slowed for tests.
#[cfg(any(test, feature = "test-log-write-delay"))]
struct SlowWrites<W>
{
    inner: W,
    delay: Option<Duration>,
}

#[cfg(any(test, feature = "test-log-write-delay"))]
impl<W: io::Write> io::Write for SlowWrites<W>
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>
    {
        if let Some(delay) = self.delay {
            std::thread::sleep(delay);
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()>
    {
        self.inner.flush()
    }
}

/// Initialize logging for TUI mode (file-only, no stdout)
///
/// This function configures logging to write only to a file, not to stdout/stderr,
/// which prevents log messages from interfering with the TUI display.
///
/// The log file is `YYYY-MM-DD-ferros-tui.log` in [`log_directory`]. The file is
/// written synchronously, so no record is lost at exit and the caller holds nothing.
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
    init_synchronous_file_logging("ferros-tui", level, LogFormat::Pretty)
}

/// Initialize file-only logging for a caller that cannot hold a [`LoggingSession`].
///
/// Same file as [`start_file_logging`], but written synchronously: each record
/// reaches the file before the logging call returns, so nothing needs flushing at
/// exit. Prefer [`start_file_logging`] where the caller can own a value.
///
/// ## Errors
///
/// Returns an error if logging is already initialized or the log file cannot be opened.
pub fn init_logging_to_file(level: Option<LogLevel>, format: LogFormat) -> Result<PathBuf, LoggingError>
{
    init_synchronous_file_logging("ferros", level, format)
}

fn init_synchronous_file_logging(stem: &str, level: Option<LogLevel>, format: LogFormat) -> Result<PathBuf, LoggingError>
{
    let file = open_log_directory()?.join(dated_file_name(stem));
    let appender = open_appender(&file, Rotation::NEVER)?;
    install_file_only(Mutex::new(appender), format, level.map(Into::into))?;
    Ok(file)
}

fn init_logging_internal(format: LogFormat, default_level: Level) -> Result<(), LoggingError>
{
    // Build environment filter
    // RUST_LOG can override the default level with more specific filters
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level.to_string()));

    // Check if file logging is requested. The file is written synchronously: this
    // entry point returns no value to hold, so a background worker could not be flushed.
    let log_file = env::var("FERROS_LOG_FILE")
        .ok()
        .map(PathBuf::from)
        .map(|path| open_appender(&path, Rotation::DAILY).map(Mutex::new))
        .transpose()?;

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

            if let Some(appender) = log_file {
                // File logging with pretty format
                let file_layer = fmt::layer()
                    .with_writer(appender)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_timer(ChronoUtc::rfc_3339())
                    .with_ansi(false) // No ANSI in files
                    .with_filter(env_filter);

                install(Registry::default().with(console_layer).with(file_layer))?;
            } else {
                // Console only
                install(Registry::default().with(console_layer))?;
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

            if let Some(appender) = log_file {
                // File logging with JSON format
                let file_layer = fmt::layer()
                    .json()
                    .with_writer(appender)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_timer(ChronoUtc::rfc_3339())
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_filter(env_filter);

                install(Registry::default().with(console_layer).with(file_layer))?;
            } else {
                // Console only
                install(Registry::default().with(console_layer))?;
            }
        }
    }

    Ok(())
}

fn initialization_failed(error: impl std::fmt::Display) -> LoggingError
{
    LoggingError::InitializationFailed(error.to_string())
}

/// Install `subscriber` as the global default, then forward `log` records to it.
///
/// The `log` bridge is optional: when another `log` logger is already installed,
/// that logger keeps the `log` records and the tracing subscriber still works.
/// Installing both in one step would leave the subscriber installed but report
/// failure, so the caller would drop its worker and lose every later record.
fn install<S>(subscriber: S) -> Result<(), LoggingError>
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    tracing::subscriber::set_global_default(subscriber).map_err(initialization_failed)?;
    let _ = tracing_log::LogTracer::builder()
        .with_max_level(tracing::level_filters::LevelFilter::current().as_log())
        .init();
    Ok(())
}

/// Install a global file-only subscriber that writes to `writer`.
fn install_file_only<W>(writer: W, format: LogFormat, explicit_level: Option<Level>) -> Result<(), LoggingError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
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
            let file_layer = fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(ChronoUtc::rfc_3339())
                .with_ansi(false) // No ANSI in files
                .with_filter(env_filter);

            install(Registry::default().with(file_layer))
        }
        LogFormat::Json => {
            let file_layer = fmt::layer()
                .json()
                .with_writer(writer)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(ChronoUtc::rfc_3339())
                .with_current_span(true)
                .with_span_list(true)
                .with_filter(env_filter);

            install(Registry::default().with(file_layer))
        }
    }
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

#[cfg(test)]
mod session_tests
{
    //! Subprocess tests: each one installs a global subscriber, so each runs in a
    //! child process and never touches the test runner's own subscriber.

    use std::path::Path;
    use std::process::{Command, Output};

    use super::*;

    /// Milliseconds the worker holds each record: well under the guard's
    /// one-second flush bound, far longer than the child takes to exit.
    const SLOW_WRITE_MS: &str = "250";

    /// A `log` logger that some other library installed first.
    struct OtherLogger;

    impl tracing_log::log::Log for OtherLogger
    {
        fn enabled(&self, _: &tracing_log::log::Metadata<'_>) -> bool
        {
            true
        }

        fn log(&self, _: &tracing_log::log::Record<'_>) {}

        fn flush(&self) {}
    }

    const MODE: &str = "FERROS_LOG_TEST_MODE";
    const REPORT: &str = "FERROS_LOG_TEST_REPORT";
    const CHILD: &str = "logging::session_tests::child";

    /// A fresh empty directory for one test.
    fn scratch(name: &str) -> PathBuf
    {
        let directory = env::temp_dir().join(format!("ferros-logging-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// Re-run this test binary on `child` with `mode`, `HOME=home`, and a report file.
    fn run_child(mode: &str, home: &Path, report: &Path) -> Output
    {
        Command::new(env::current_exe().unwrap())
            .args(["--exact", CHILD, "--nocapture", "--test-threads=1"])
            .env(MODE, mode)
            .env(REPORT, report)
            .env("HOME", home)
            .env(TEST_WRITE_DELAY, SLOW_WRITE_MS)
            .env_remove("RUST_LOG")
            .output()
            .unwrap()
    }

    fn assert_clean_exit(output: &Output)
    {
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The only file in `directory`, read to a string.
    fn only_log(directory: &Path) -> String
    {
        let mut files = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1, "expected one log file in {}: {files:?}", directory.display());
        std::fs::read_to_string(files.pop().unwrap()).unwrap()
    }

    /// The child side. Does nothing unless a parent test set the mode.
    #[test]
    fn child()
    {
        let Ok(mode) = env::var(MODE) else { return };
        let report = PathBuf::from(env::var_os(REPORT).unwrap());
        match mode.as_str() {
            "emit-pretty" | "emit-json" => {
                let format = if mode.ends_with("json") {
                    LogFormat::Json
                } else {
                    LogFormat::Pretty
                };
                let session = start_file_logging(None, format).unwrap();
                std::fs::write(&report, session.directory().as_os_str().as_encoded_bytes()).unwrap();
                // The slowed worker is still holding this record when `drop` runs.
                tracing::info!("flush-me");
                // Returning from `main` drops the session, then the process exits at
                // once. Nothing may be lost between the two.
                drop(session);
                std::process::exit(0);
            }
            "other-log-logger" => {
                tracing_log::log::set_logger(&OtherLogger).unwrap();
                let session = start_file_logging(None, LogFormat::Pretty);
                let session = session.expect("an existing `log` logger must not stop tracing from installing");
                tracing::info!("flush-me");
                session.finish();
                std::process::exit(0);
            }
            "second-install" => {
                let _first = start_file_logging(None, LogFormat::Pretty).unwrap();
                let second = start_file_logging(None, LogFormat::Json);
                assert!(
                    matches!(second, Err(LoggingError::InitializationFailed(_))),
                    "second install: {second:?}"
                );
                let sync = init_logging_to_file(None, LogFormat::Pretty);
                assert!(
                    matches!(sync, Err(LoggingError::InitializationFailed(_))),
                    "synchronous install: {sync:?}"
                );
            }
            "unwritable" => {
                let result = start_file_logging(None, LogFormat::Pretty);
                assert!(matches!(result, Err(LoggingError::FileError(_))), "unwritable: {result:?}");
            }
            "synchronous" => {
                let file = init_logging_to_file(None, LogFormat::Pretty).unwrap();
                std::fs::write(&report, file.as_os_str().as_encoded_bytes()).unwrap();
                tracing::info!("flush-me");
                // No guard exists, and the process exits at once.
                std::process::exit(0);
            }
            other => panic!("unknown mode {other}"),
        }
    }

    fn assert_flushed(mode: &str, name: &str, json: bool)
    {
        let home = scratch(name);
        let report = home.join("report");
        let output = run_child(mode, &home, &report);
        assert_clean_exit(&output);

        let directory = home.join(".ferros");
        assert_eq!(
            std::fs::read(&report).unwrap(),
            directory.as_os_str().as_encoded_bytes(),
            "the session opened a different directory than the one find-logs prints"
        );
        let log = only_log(&directory);
        assert!(log.contains("flush-me"), "log file is missing the last record: {log:?}");
        assert_eq!(log.contains("\"message\":\"flush-me\""), json, "format: {log:?}");

        // File-only logging leaves the terminal transcript clean.
        let terminal = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        assert!(!terminal.contains("flush-me"), "record reached the terminal: {terminal}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn file_log_contains_the_record_written_just_before_exit()
    {
        assert_flushed("emit-pretty", "flush-pretty", false);
    }

    #[test]
    fn json_file_log_contains_the_record_written_just_before_exit()
    {
        assert_flushed("emit-json", "flush-json", true);
    }

    #[test]
    fn an_existing_log_logger_leaves_tracing_working()
    {
        let home = scratch("other-log");
        assert_clean_exit(&run_child("other-log-logger", &home, &home.join("report")));
        let log = only_log(&home.join(".ferros"));
        assert!(log.contains("flush-me"), "log file is missing the record: {log:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn synchronous_file_log_needs_no_guard()
    {
        let home = scratch("synchronous");
        let report = home.join("report");
        assert_clean_exit(&run_child("synchronous", &home, &report));
        let log = only_log(&home.join(".ferros"));
        assert!(log.contains("flush-me"), "log file is missing the record: {log:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_second_installation_returns_an_error()
    {
        let home = scratch("second");
        assert_clean_exit(&run_child("second-install", &home, &home.join("report")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_unwritable_destination_returns_an_error()
    {
        let home = scratch("unwritable");
        // `.ferros` is a regular file, so the directory cannot be created.
        std::fs::write(home.join(".ferros"), b"not a directory").unwrap();
        assert_clean_exit(&run_child("unwritable", &home, &home.join("report")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_relative_or_empty_directory_is_made_absolute()
    {
        let cwd = env::current_dir().unwrap();
        assert_eq!(absolute_directory(PathBuf::from("logs")), cwd.join("logs"));
        assert_eq!(absolute_directory(PathBuf::new()), PathBuf::from(EMPTY_TEMP_DIR));
        assert!(absolute_directory(PathBuf::new()).is_absolute());
        assert_eq!(absolute_directory(PathBuf::from("/a/b")), PathBuf::from("/a/b"));
    }

    #[test]
    fn the_directory_is_home_ferros_or_the_temp_directory()
    {
        // Pure function of HOME; checked in a child so the test runner's environment is untouched.
        let home = scratch("directory");
        let report = home.join("report");
        assert_clean_exit(&run_child("emit-pretty", &home, &report));
        assert_eq!(
            PathBuf::from(std::ffi::OsString::from(
                String::from_utf8(std::fs::read(&report).unwrap()).unwrap()
            )),
            home.join(".ferros")
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
