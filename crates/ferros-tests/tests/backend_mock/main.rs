//! The portable `Backend` trait and its in-memory `MockBackend`, used from outside
//! `ferros-core` through the public API only (the `test-util` feature).
//!
//! Unlike every other file here, these tests launch no real process: the mock is a
//! scripted, in-memory backend. They therefore need no `harness::run`, no fixtures and no
//! debugger entitlement, and they are not limited to macOS.

mod build_ids;
mod carried_signals;
mod handler_exits;
mod held_signals;
mod helpers;
mod image_enumeration;
mod injected_runtime;
mod late_build_ids;
mod lifecycle;
mod line_breakpoints;
mod logical_breakpoints;
mod stale_return_traps;
mod step_out_traps;
mod step_over_traps;
mod target_arch;
mod values;
