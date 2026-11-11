//! Integration tests verifying fixture compilation, execution, markers, and debug symbols.

use std::process::Command;

use ferros_tests::fixture;

#[test]
fn test_fixture_hello()
{
    let fix = fixture!("hello");
    assert!(fix.path().is_file(), "binary does not exist: {}", fix.path().display());

    let output = Command::new(fix.path())
        .arg("--test-arg")
        .output()
        .expect("failed to execute hello fixture");
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello from ferros fixture"), "unexpected stdout: {stdout}");

    let marker = fix.marker_line("hello_print");
    assert!(marker > 0);

    #[cfg(target_os = "macos")]
    assert!(
        fix.dsym_path().exists(),
        "dSYM bundle missing at {}",
        fix.dsym_path().display()
    );
}

#[test]
fn test_fixture_exit_code()
{
    let fix = fixture!("exit_code");
    assert!(fix.path().is_file(), "binary does not exist: {}", fix.path().display());

    let output = Command::new(fix.path())
        .output()
        .expect("failed to execute exit_code fixture");
    assert_eq!(output.status.code(), Some(7));

    let marker = fix.marker_line("before_exit");
    assert!(marker > 0);

    #[cfg(target_os = "macos")]
    assert!(
        fix.dsym_path().exists(),
        "dSYM bundle missing at {}",
        fix.dsym_path().display()
    );
}

#[test]
fn test_fixture_loop_counter()
{
    let fix = fixture!("loop_counter");
    assert!(fix.path().is_file(), "binary does not exist: {}", fix.path().display());

    let output = Command::new(fix.path())
        .arg("3")
        .output()
        .expect("failed to execute loop_counter fixture");
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("final count: 3"), "unexpected stdout: {stdout}");

    let loop_call = fix.marker_line("loop_call");
    let tick_body = fix.marker_line("tick_body");
    assert!(loop_call > 0);
    assert!(tick_body > 0);
    assert_ne!(loop_call, tick_body);

    #[cfg(target_os = "macos")]
    assert!(
        fix.dsym_path().exists(),
        "dSYM bundle missing at {}",
        fix.dsym_path().display()
    );
}

#[test]
fn test_fixture_nested_calls()
{
    let fix = fixture!("nested_calls");
    assert!(fix.path().is_file(), "binary does not exist: {}", fix.path().display());

    let output = Command::new(fix.path())
        .output()
        .expect("failed to execute nested_calls fixture");
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nested result:"), "unexpected stdout: {stdout}");

    let in_a = fix.marker_line("in_a");
    let in_b = fix.marker_line("in_b");
    let in_c = fix.marker_line("in_c");
    let after_calls = fix.marker_line("after_calls");

    assert!(in_a > 0);
    assert!(in_b > 0);
    assert!(in_c > 0);
    assert!(after_calls > 0);

    #[cfg(target_os = "macos")]
    assert!(
        fix.dsym_path().exists(),
        "dSYM bundle missing at {}",
        fix.dsym_path().display()
    );
}

#[test]
fn test_fixture_threads()
{
    let fix = fixture!("threads");
    assert!(fix.path().is_file(), "binary does not exist: {}", fix.path().display());

    let output = Command::new(fix.path()).output().expect("failed to execute threads fixture");
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("threads finished:"), "unexpected stdout: {stdout}");

    let worker_body = fix.marker_line("worker_body");
    let after_join = fix.marker_line("after_join");
    assert!(worker_body > 0);
    assert!(after_join > 0);
    assert_ne!(worker_body, after_join);

    #[cfg(target_os = "macos")]
    assert!(
        fix.dsym_path().exists(),
        "dSYM bundle missing at {}",
        fix.dsym_path().display()
    );
}
