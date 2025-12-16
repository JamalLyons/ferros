use std::env;
use std::path::PathBuf;

use crate::error::{ConfigError, Result};

/// Resolve the global configuration file path.
///
/// By convention this is `$HOME/.config/ferros/config.toml`.
pub fn global_config_path() -> Result<PathBuf>
{
    let home = env::var("HOME").map_err(|source| ConfigError::HomeEnv { source })?;
    Ok(PathBuf::from(home).join(".config").join("ferros").join("config.toml"))
}

/// Resolve the local configuration file path in the current directory.
///
/// This is always `./config.toml` relative to the current working directory.
pub fn local_config_path() -> Result<PathBuf>
{
    let cwd = env::current_dir().map_err(|source| ConfigError::CurrentDir { source })?;
    Ok(cwd.join("config.toml"))
}

/// Resolve the configuration path from the `FERROS_CONFIG` environment variable, if set.
pub fn env_config_path() -> Option<PathBuf>
{
    env::var("FERROS_CONFIG").ok().map(PathBuf::from)
}
