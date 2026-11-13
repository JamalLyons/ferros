//! Workers raise SIGUSR1 independently; the observer records which handlers ran.
#[cfg(target_os = "macos")]
fn main()
{
    use std::time::Duration;

    use ferros_macos::diagnostics;
    let directory = std::path::PathBuf::from(std::env::args().nth(1).expect("observer directory"));
    diagnostics::install_sigusr1_observer().expect("SIGUSR1 observer");
    for slot in 0..2 {
        let directory = directory.clone();
        std::thread::spawn(move || {
            diagnostics::register_signal_worker(slot);
            std::fs::write(directory.join(format!("ready-{slot}")), b"ready").expect("worker ready");
            while !directory.join(format!("signal-{slot}")).exists() {
                std::thread::sleep(Duration::from_millis(1));
            }
            diagnostics::raise_sigusr1().expect("raise worker SIGUSR1");
            std::fs::write(directory.join(format!("done-{slot}")), b"returned").expect("worker returned");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }
    loop {
        let handled = diagnostics::signal_workers_handled();
        for slot in 0..2 {
            if handled & (1 << slot) != 0 {
                std::fs::write(directory.join(format!("handled-{slot}")), b"handled").expect("handler acknowledgement");
            }
        }
        if directory.join("snapshot").exists() {
            std::fs::write(directory.join("observed"), handled.to_string()).expect("handler snapshot");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {}
