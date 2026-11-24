//! `ferros find-logs` prints the directory the logger opens.

use std::process::Command;

#[test]
fn find_logs_prints_the_directory_a_session_logs_to()
{
    let home = std::env::temp_dir().join(format!("ferros-find-logs-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferros"))
        .arg("find-logs")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim_end(),
        home.join(".ferros").display().to_string()
    );

    // With no home, both fall back to the OS temporary directory.
    let output = Command::new(env!("CARGO_BIN_EXE_ferros"))
        .arg("find-logs")
        .env_remove("HOME")
        .output()
        .unwrap();
    assert_eq!(
        std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end()),
        std::env::temp_dir()
    );
    let _ = std::fs::remove_dir_all(&home);
}
