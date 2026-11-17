//! Reports when the debugger's detach SIGCONT invokes a real signal handler.
#[cfg(target_os = "macos")]
fn main()
{
    if std::env::args().any(|arg| arg == "--step-resume") {
        signal_roundtrip();
        return;
    }
    ferros_macos::diagnostics::install_sigcont_observer().expect("install SIGCONT handler");
    println!("ready");
    while !ferros_macos::diagnostics::sigcont_observed() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("continued");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
#[inline(never)]
fn signal_roundtrip()
{
    use ferros_macos::diagnostics;
    diagnostics::install_sigusr1_observer().expect("install SIGUSR1 handler");
    diagnostics::raise_sigusr1().expect("raise SIGUSR1");
    let handled = std::hint::black_box(diagnostics::sigusr1_observed()); // BREAK: after_signal
    println!("signal handled: {handled}");
}
