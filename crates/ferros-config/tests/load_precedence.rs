use std::path::PathBuf;
use std::{env, fs};

use ferros_config::{Config, ConfigSource, DebuggerConfig, LogLevel};

fn write_config(path: &PathBuf, level: LogLevel)
{
    let cfg = Config {
        debugger: DebuggerConfig {
            log_level: level,
            default_attach_mode: None,
        },
    };
    cfg.write_to_path(path).unwrap();
}

#[test]
fn uses_env_override_when_set()
{
    let temp_dir = tempfile::tempdir().unwrap();
    let env_path = temp_dir.path().join("env-config.toml");

    write_config(&env_path, LogLevel::Debug);

    // SAFETY: Test-only environment mutation; isolated to this process.
    unsafe {
        env::set_var("FERROS_CONFIG", &env_path);
    }

    let (config, source) = Config::load().unwrap();

    assert_eq!(source, ConfigSource::Env);
    assert!(matches!(config.debugger.log_level, LogLevel::Debug));

    // SAFETY: Test-only environment mutation; isolated to this process.
    unsafe {
        env::remove_var("FERROS_CONFIG");
    }
}

#[test]
fn prefers_local_over_global_when_no_env()
{
    // Ensure env override is not set for this test
    // SAFETY: Test-only environment mutation; isolated to this process.
    unsafe {
        env::remove_var("FERROS_CONFIG");
    }

    let temp_home = tempfile::tempdir().unwrap();
    let temp_cwd = tempfile::tempdir().unwrap();

    // Point HOME and current_dir to temporary locations
    // SAFETY: Test-only environment mutation; isolated to this process.
    unsafe {
        env::set_var("HOME", temp_home.path());
    }
    env::set_current_dir(&temp_cwd).unwrap();

    let global_path = PathBuf::from(env::var("HOME").unwrap())
        .join(".config")
        .join("ferros")
        .join("config.toml");
    let local_path = temp_cwd.path().join("config.toml");

    write_config(&global_path, LogLevel::Warn);
    write_config(&local_path, LogLevel::Trace);

    let (config, source) = Config::load().unwrap();

    assert_eq!(source, ConfigSource::Local);
    assert!(matches!(config.debugger.log_level, LogLevel::Trace));
}

#[test]
fn creates_global_default_when_none_exist()
{
    let temp_home = tempfile::tempdir().unwrap();
    let temp_cwd = tempfile::tempdir().unwrap();

    // SAFETY: Test-only environment mutation; isolated to this process.
    unsafe {
        env::set_var("HOME", temp_home.path());
    }
    env::set_current_dir(&temp_cwd).unwrap();
    // SAFETY: Test-only environment mutation; isolated to this process.
    unsafe {
        env::remove_var("FERROS_CONFIG");
    }

    let global_path = PathBuf::from(env::var("HOME").unwrap())
        .join(".config")
        .join("ferros")
        .join("config.toml");

    assert!(!global_path.exists());

    let (config, source) = Config::load().unwrap();

    assert_eq!(source, ConfigSource::GlobalDefaultCreated);
    assert!(matches!(config.debugger.log_level, LogLevel::Info));
    assert!(global_path.exists());

    let contents = fs::read_to_string(global_path).unwrap();
    assert!(contents.contains("[debugger]"));
}
