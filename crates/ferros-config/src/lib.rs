//! Ferros configuration crate
//!
//! This crate provides configuration loading for the Ferros debugger.
//! It discovers a `config.toml` file using the following precedence:
//!
//! 1. `FERROS_CONFIG` environment variable (if set)
//! 2. `config.toml` in the current working directory
//! 3. Global config: `$HOME/.config/ferros/config.toml`
//!
//! If no configuration file is found, the default configuration is
//! written to the global config path and returned.

mod config;
mod error;
mod paths;

use std::path::Path;
use std::{fs, io};

pub use crate::config::{Config, DebuggerConfig, LogLevel};
pub use crate::error::{ConfigError, Result};

/// Indicates where the configuration was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource
{
    /// Configuration loaded from the `FERROS_CONFIG` environment variable.
    Env,
    /// Configuration loaded from `config.toml` in the current working directory.
    Local,
    /// Configuration loaded from the global config file.
    Global,
    /// No configuration file existed; defaults were written to the global config path.
    GlobalDefaultCreated,
}

impl Config
{
    /// Load configuration using the standard Ferros lookup order.
    ///
    /// Lookup order:
    /// 1. `FERROS_CONFIG` (if set)
    /// 2. `config.toml` in the current directory
    /// 3. `$HOME/.config/ferros/config.toml`
    ///
    /// If no configuration exists, the default configuration is written
    /// to the global config path and returned.
    pub fn load() -> Result<(Self, ConfigSource)>
    {
        // 1. Environment override
        if let Some(env_path) = paths::env_config_path() {
            let config = Self::load_from(&env_path)?;
            return Ok((config, ConfigSource::Env));
        }

        // 2. Local config in current directory
        let local_path = paths::local_config_path()?;
        match Self::load_from(&local_path) {
            Ok(config) => return Ok((config, ConfigSource::Local)),
            Err(error::ConfigError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                // No local config; continue to global lookup.
            }
            Err(e) => return Err(e),
        }

        // 3. Global config in $HOME/.config/ferros/config.toml
        let global_path = paths::global_config_path()?;
        match Self::load_from(&global_path) {
            Ok(config) => return Ok((config, ConfigSource::Global)),
            Err(error::ConfigError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                // No global config; we'll create one with defaults below.
            }
            Err(e) => return Err(e),
        }

        // No config found anywhere; write defaults to global path
        let default_config = Self::default();
        default_config.write_to_path(&global_path)?;
        Ok((default_config, ConfigSource::GlobalDefaultCreated))
    }

    /// Load configuration from an explicit path.
    pub fn load_from(path: &Path) -> Result<Self>
    {
        let contents = fs::read_to_string(path).map_err(|source| error::ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        toml::from_str(&contents).map_err(|source| error::ConfigError::Toml {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Write this configuration to the specified path as TOML.
    ///
    /// Parent directories are created if they do not exist.
    pub fn write_to_path<P: AsRef<Path>>(&self, path: P) -> Result<()>
    {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| error::ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let toml_string = toml::to_string_pretty(self).map_err(|source| error::ConfigError::TomlSerialize { source })?;

        fs::write(path, toml_string).map_err(|source| error::ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
