//! Non-local exits by `longjmp` and `siglongjmp` (issue #141).
//!
//! The program is C, in `fixtures/c/long_jump.c`, compiled and linked into this fixture
//! only by `build.rs`: `setjmp` returns twice, which Rust cannot call soundly. That file
//! defines `main` and holds the markers; tests read them with `ferros_tests::find_marker`.
#![no_main]
