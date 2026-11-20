//! Individually gated workers for native stepping and retained-signal tests.

#[inline(never)]
fn step_site(slot: usize) -> usize
{
    std::hint::black_box(slot) // BREAK: step_site
}

#[cfg(target_os = "macos")]
fn main()
{
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use ferros_macos::diagnostics;

    let mut args = std::env::args().skip(1);
    let directory = std::path::PathBuf::from(args.next().expect("control directory"));
    let (mut reader, writer) = if args.any(|arg| arg == "--block-in-read") {
        let (reader, writer) = UnixStream::pair().expect("step socket pair");
        (Some(reader), Some(writer))
    } else {
        (None, None)
    };
    diagnostics::install_sigusr1_observer().expect("SIGUSR1 observer");
    let mut workers = Vec::new();
    for slot in 0..2 {
        let directory = directory.clone();
        let reader = reader.take();
        workers.push(std::thread::spawn(move || {
            diagnostics::register_signal_worker(slot);
            let id = diagnostics::current_thread_unique_id().to_string();
            std::fs::write(directory.join(format!("tid-{slot}")), id).expect("thread id");
            std::fs::write(directory.join(format!("ready-{slot}")), b"ready").expect("ready");
            while !directory.join(format!("run-{slot}")).exists() || !directory.join(format!("ready-{}", 1 - slot)).exists()
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::hint::black_box(step_site(slot));
            #[cfg(target_arch = "aarch64")]
            if let Some(reader) = reader {
                assert_eq!(diagnostics::read_step_fixture_byte(&reader).expect("blocking read"), b'x');
            }
            #[cfg(not(target_arch = "aarch64"))]
            drop(reader);
            if directory.join(format!("signal-{slot}")).exists() {
                diagnostics::raise_sigusr1().expect("worker signal");
            }
            std::fs::write(directory.join(format!("done-{slot}")), b"done").expect("done");
            while !directory.join("exit").exists() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    while !directory.join("exit").exists() {
        if directory.join("snapshot").exists() && !directory.join("observed").exists() {
            std::fs::write(
                directory.join("observed.tmp"),
                diagnostics::signal_workers_handled().to_string(),
            )
            .expect("handler snapshot");
            std::fs::rename(directory.join("observed.tmp"), directory.join("observed")).expect("publish snapshot");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if let Some(mut writer) = writer {
        writer.write_all(b"x").expect("release blocking read");
    }
    for worker in workers {
        worker.join().expect("worker");
    }
}

#[cfg(not(target_os = "macos"))]
fn main()
{
    std::hint::black_box(step_site(0));
}
