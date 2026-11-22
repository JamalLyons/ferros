//! `pthread_once` into a static function of a dyld shared-cache library (issue #121).
//!
//! The program is C, in `fixtures/c/pthread_once.c`, compiled and linked into this fixture
//! only by `build.rs`. That file defines `main` and holds the markers; tests read them
//! with `ferros_tests::find_marker`.
//!
//! `build.rs` builds the C program only for arm64 macOS. Elsewhere this binary is a stub
//! with a Rust `main`, so the workspace still links there (issue #160).
#![cfg_attr(all(target_os = "macos", target_arch = "aarch64"), no_main)]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main()
{
    eprintln!("the pthread_once fixture is built only for arm64 macOS");
    std::process::exit(1);
}
