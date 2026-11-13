//! Two live threads and a filesystem acknowledgement of a handled application signal.
#[cfg(target_os = "macos")]
fn main()
{
    use std::time::Duration;
    let directory = std::env::args().nth(1).expect("observer directory");
    ferros_macos::diagnostics::install_sigusr1_observer().expect("SIGUSR1 observer");
    let (ready, worker) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        ready.send(()).expect("worker ready");
        loop {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    worker.recv().expect("worker started");
    std::fs::write(format!("{directory}/ready"), b"ready").expect("ready file");
    loop {
        if ferros_macos::diagnostics::sigusr1_observed() {
            std::fs::write(format!("{directory}/handled"), b"handled").expect("handled file");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {}
