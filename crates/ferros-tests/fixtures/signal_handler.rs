//! Reports when the debugger's detach SIGCONT invokes a real signal handler.
#[cfg(target_os = "macos")]
fn main()
{
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
