use serde::{Deserialize, Serialize};

/// Top-level Ferros configuration.
///
/// This structure maps directly to the `config.toml` file used by Ferros.
///
/// # Example
///
/// ```toml
/// [debugger]
/// log_level = "info" # one of: error, warn, info, debug, trace
/// # default_attach_mode = "launch"  # reserved for future use
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config
{
    /// Debugger-related configuration.
    #[serde(default)]
    pub debugger: DebuggerConfig,
}

/// Debugger-specific configuration section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebuggerConfig
{
    /// Default log level used by the debugger if not overridden by CLI.
    #[serde(default)]
    pub log_level: LogLevel,

    /// Reserved for future use: default attach mode (e.g. "attach", "launch").
    #[serde(default)]
    pub default_attach_mode: Option<String>,
}

impl Default for DebuggerConfig
{
    fn default() -> Self
    {
        Self {
            log_level: LogLevel::Info,
            default_attach_mode: None,
        }
    }
}

/// Log level used for Ferros logging.
///
/// This is intentionally close to the log levels supported by `ferros-utils`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel
{
    /// Error-level logging.
    Error,
    /// Warning-level logging.
    Warn,
    /// Informational logging (default).
    #[default]
    Info,
    /// Debug-level logging.
    Debug,
    /// Trace-level logging.
    Trace,
}
