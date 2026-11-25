//! A null store, one counted soft signal, and an `EXC_SOFTWARE` exception that is not a
//! Unix signal (issue #244).
//!
//! The program is C, in `fixtures/c/soft_faults.c`, compiled and linked into this fixture
//! only by `build.rs`: a Rust fixture cannot store through a null pointer or build a Mach
//! message without `unsafe`. That file defines `main` and holds the markers; tests read
//! them with `ferros_tests::find_marker`.
//!
//! `build.rs` builds the C program only for arm64 macOS. Elsewhere this binary is a stub
//! with a Rust `main`, so the workspace still links there (issue #160).
#![cfg_attr(all(target_os = "macos", target_arch = "aarch64"), no_main)]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main()
{
    eprintln!("the soft_faults fixture is built only for arm64 macOS");
    std::process::exit(1);
}
