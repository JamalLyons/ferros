use std::path::PathBuf;
use std::{env, io};

use thiserror::Error;

/// Result type for configuration operations.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Errors that can occur while working with Ferros configuration.
#[derive(Debug, Error)]
pub enum ConfigError
{
    /// I/O error while reading or writing a configuration file.
    #[error("I/O error while accessing config at {path:?}: {source}")]
    Io
    {
        /// Path to the file that failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Error while parsing a TOML configuration file.
    #[error("Failed to parse TOML config at {path:?}: {source}")]
    Toml
    {
        /// Path to the TOML file.
        path: PathBuf,
        /// Underlying TOML deserialization error.
        #[source]
        source: toml::de::Error,
    },

    /// Error while serializing configuration to TOML.
    #[error("Failed to serialize configuration to TOML: {source}")]
    TomlSerialize
    {
        /// Underlying TOML serialization error.
        #[source]
        source: toml::ser::Error,
    },

    /// The HOME environment variable was not set or not valid UTF-8.
    #[error("Failed to determine HOME directory from environment: {source}")]
    HomeEnv
    {
        /// Underlying environment error.
        #[source]
        source: env::VarError,
    },

    /// The current working directory could not be determined.
    #[error("Failed to determine current working directory: {source}")]
    CurrentDir
    {
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}
