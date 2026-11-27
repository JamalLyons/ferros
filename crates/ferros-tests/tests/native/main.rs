//! Integration tests that debug real fixture processes, one module per area.
//!
//! One binary instead of one per area: each test binary links `ferros-core` and
//! `ferros-macos`, so a single binary keeps an engine change to one link (issue #294).
//! Run one area with `cargo test -p ferros-tests --test native <area>::`. Each module keeps
//! its own `#![cfg(target_os = "macos")]`.
//!
//! `tests/exceptions.rs` stays a separate binary: two of its tests count this process's
//! whole IPC space and would see the ports of tests in other modules.

mod backtrace;
mod breakpoints;
mod cli;
mod code_writes;
mod debuginfo;
mod error_codes;
mod exit_watcher;
mod fixture_staging;
mod fixtures;
mod frames;
mod harness;
mod images;
mod lifecycle;
mod macos_crate;
mod memory_reads;
mod no_debug_info;
mod non_local_exits;
mod process_group;
mod repl_pty;
mod signal_tracing;
mod single_step;
mod smoke;
mod source_breakpoints;
mod stepping;
mod symbols;
mod types;
mod universal_images;
mod values;
mod values_collections;
mod values_dyn;
mod values_enums;
mod values_ffi_strings;
mod values_maps;
mod values_misc;
mod values_newtypes;
mod values_paths;
mod values_shared;
mod values_std;
mod values_sync;
