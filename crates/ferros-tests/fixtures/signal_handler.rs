//! Reports when the debugger's detach SIGCONT invokes a real signal handler.
//!
//! With `--step-resume`, raises SIGUSR1 at itself and stops after the handler
//! (marker `after_signal`).
//!
//! With `--interrupt`, installs `on_sigusr1` for SIGUSR1 and spins forever in `spin` on
//! one line with no calls (marker `spin_line`, called from `main` at `call_spin`), so a
//! SIGUSR1 sent from outside interrupts `spin` on that line. The handler is a named,
//! non-inlined function (marker `in_handler`). Used for backtraces through `_sigtramp`.
#[cfg(target_os = "macos")]
fn main()
{
    if std::env::args().any(|arg| arg == "--step-resume") {
        signal_roundtrip();
        return;
    }
    if std::env::args().any(|arg| arg == "--interrupt") {
        ferros_macos::diagnostics::install_sigusr1_handler(on_sigusr1).expect("install SIGUSR1 handler");
        spin(); // BREAK: call_spin
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

/// Plain arithmetic only: every instruction of the loop belongs to the marked line.
#[cfg(target_os = "macos")]
#[rustfmt::skip]
#[inline(never)]
fn spin() -> !
{
    let mut spins: u64 = 0;
    loop { spins = spins.wrapping_add(1) | 1; } // BREAK: spin_line
}

#[cfg(target_os = "macos")]
#[inline(never)]
extern "C" fn on_sigusr1(signal: i32)
{
    let _ = std::hint::black_box(signal); // BREAK: in_handler
}
