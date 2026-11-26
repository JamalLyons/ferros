//! Two live threads and a filesystem acknowledgement of a handled application signal.
//!
//! Flags after the observer directory: `--own-group` moves the fixture into a process
//! group of its own, so job-control stop signals reach it rather than being discarded
//! for an orphaned group; `--catch-tstp` handles SIGTSTP too, and the fixture writes
//! `tstp` once its handler has run.
#[cfg(target_os = "macos")]
fn main()
{
    use std::time::Duration;
    let directory = std::env::args().nth(1).expect("observer directory");
    ferros_macos::diagnostics::install_sigusr1_observer().expect("SIGUSR1 observer");
    ferros_macos::diagnostics::install_sigcont_observer().expect("SIGCONT observer");
    let flags: Vec<String> = std::env::args().skip(2).collect();
    if flags.iter().any(|flag| flag == "--own-group") {
        ferros_macos::diagnostics::enter_own_process_group().expect("own process group");
    }
    if flags.iter().any(|flag| flag == "--catch-tstp") {
        ferros_macos::diagnostics::install_sigtstp_observer().expect("SIGTSTP observer");
    }
    let (ready, worker) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        ready.send(()).expect("worker ready");
        loop {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    worker.recv().expect("worker started");
    std::fs::write(format!("{directory}/ready"), b"ready").expect("ready file");
    let mut progress = 0_u64;
    loop {
        progress += 1;
        std::fs::write(format!("{directory}/progress"), progress.to_string()).expect("progress file");
        if ferros_macos::diagnostics::sigusr1_observed() {
            std::fs::write(format!("{directory}/handled"), b"handled").expect("handled file");
        }
        if ferros_macos::diagnostics::sigtstp_observed() {
            std::fs::write(format!("{directory}/tstp"), b"tstp").expect("tstp file");
        }
        if ferros_macos::diagnostics::sigcont_observed() {
            std::fs::write(format!("{directory}/continued"), b"continued").expect("continued file");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {}
