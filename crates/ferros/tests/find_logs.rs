//! `ferros find-logs` prints the directory the logger opens, and the CLI keeps its
//! logging session until it exits, so the last record reaches the file.
//!
//! Every test drives the real binary. `launch --stdin <missing file>` starts file
//! logging and then fails before anything is launched, so no process is debugged.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Test-only switch that delays background log writes by the given milliseconds.
const WRITE_DELAY: &str = "FERROS_TEST_LOG_WRITE_DELAY_MS";

/// A fresh empty directory for one test.
fn scratch(name: &str) -> PathBuf
{
    let directory = std::env::temp_dir().join(format!("ferros-find-logs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// `ferros <args>` in `cwd`, with `env` applied (a `None` value removes the variable).
fn ferros(args: &[&str], cwd: &Path, env: &[(&str, Option<&OsString>)]) -> Output
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferros"));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove(WRITE_DELAY)
        .env_remove("RUST_LOG")
        .env_remove("FERROS_LOG_FORMAT");
    for (name, value) in env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command.output().unwrap()
}

/// What `find-logs` printed: the raw bytes before the trailing newline.
fn printed_directory(output: &Output) -> PathBuf
{
    use std::os::unix::ffi::OsStringExt as _;

    assert!(output.status.success(), "find-logs failed: {output:?}");
    let mut bytes = output.stdout.clone();
    assert_eq!(bytes.pop(), Some(b'\n'), "find-logs output: {bytes:?}");
    PathBuf::from(OsString::from_vec(bytes))
}

/// Start file logging, then fail before launching anything; returns the CLI output.
fn log_and_exit(cwd: &Path, env: &[(&str, Option<&OsString>)]) -> Output
{
    let missing = cwd.join("missing-stdin");
    let args = vec!["launch", "--stdin", missing.to_str().unwrap(), "/bin/true"];
    let output = ferros(&args, cwd, env);
    assert!(
        !output.status.success(),
        "launch with a missing stdin file succeeded: {output:?}"
    );
    output
}

/// The `*-ferros.log` file in `directory`, read to a string.
fn ferros_log(directory: &Path) -> String
{
    let file = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with("-ferros.log"))
        .unwrap_or_else(|| panic!("no ferros log file in {}", directory.display()));
    std::fs::read_to_string(file).unwrap()
}

#[test]
fn file_log_contains_the_record_written_just_before_exit()
{
    let home = scratch("exit");
    let home_os = OsString::from(&home);
    let delay = OsString::from("250");
    let env = [("HOME", Some(&home_os)), (WRITE_DELAY, Some(&delay))];
    let output = log_and_exit(&home, &env);

    let log = ferros_log(&home.join(".ferros"));
    assert!(
        log.contains("ferros exiting"),
        "the record written as `main` returned is missing: {log:?}\n{output:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn find_logs_prints_the_directory_a_session_logs_to()
{
    let home = scratch("home");
    let home_os = OsString::from(&home);
    let env = [("HOME", Some(&home_os))];
    let printed = printed_directory(&ferros(&["find-logs"], &home, &env));
    assert_eq!(printed, home.join(".ferros"));

    log_and_exit(&home, &env);
    assert!(ferros_log(&printed).contains("ferros exiting"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn find_logs_prints_a_home_that_is_not_utf8_unchanged()
{
    use std::os::unix::ffi::OsStringExt as _;

    let cwd = scratch("non-utf8");
    let mut home = cwd.clone().into_os_string().into_vec();
    home.extend_from_slice(b"/caf\xe9");
    let home = OsString::from_vec(home);
    let printed = printed_directory(&ferros(&["find-logs"], &cwd, &[("HOME", Some(&home))]));
    assert_eq!(printed, PathBuf::from(home).join(".ferros"));
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn without_a_home_a_relative_temp_directory_is_made_absolute()
{
    let cwd = scratch("relative-tmp");
    let tmp = OsString::from("relative-tmp");
    let env = [("HOME", None), ("TMPDIR", Some(&tmp))];
    let printed = printed_directory(&ferros(&["find-logs"], &cwd, &env));
    // The child sees its working directory with symlinks resolved.
    assert_eq!(printed, cwd.canonicalize().unwrap().join("relative-tmp"));

    log_and_exit(&cwd, &env);
    assert!(ferros_log(&printed).contains("ferros exiting"));
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn without_a_home_an_empty_temp_directory_is_not_the_working_directory()
{
    let cwd = scratch("empty-tmp");
    let empty = OsString::new();
    let env = [("HOME", None), ("TMPDIR", Some(&empty))];
    // File logging uses the same directory resolver. A launch here would
    // append to a shared dated log under /tmp.
    let printed = printed_directory(&ferros(&["find-logs"], &cwd, &env));
    assert_eq!(printed, PathBuf::from("/tmp"));
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn without_a_home_find_logs_prints_the_temp_directory()
{
    let cwd = scratch("no-home");
    let printed = printed_directory(&ferros(&["find-logs"], &cwd, &[("HOME", None)]));
    assert_eq!(printed, std::path::absolute(std::env::temp_dir()).unwrap());
    let _ = std::fs::remove_dir_all(&cwd);
}
