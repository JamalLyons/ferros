//! Process lifecycle through `ferros_core::Session` on the macOS backend.
#![cfg(target_os = "macos")]

mod attached_drop;
mod detach;
mod holder;
mod launch_exit;
mod signals;

use std::process::Command;
use std::time::Duration;

use ferros_tests::harness;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
// Darwin signal numbers; the test harness deliberately uses no raw FFI.
pub(crate) const SIGUSR1: i32 = 30;
pub(crate) const SIGKILL: i32 = 9;

pub(crate) fn signal(pid: u32, name: &str)
{
    assert!(
        Command::new("/bin/kill")
            .args([name, &pid.to_string()])
            .status()
            .expect("kill command")
            .success()
    );
}

pub(crate) fn assert_reaped(pid: u32)
{
    assert!(
        harness::wait_until_gone(pid, TIMEOUT),
        "pid {pid} still exists (possibly a zombie)"
    );
}

/// The `ps` state of `pid` (for example `S`, `R` or `T`), empty if it does not exist.
pub(crate) fn process_state(pid: u32) -> String
{
    let output = Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
