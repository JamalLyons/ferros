//! Single-step and per-thread suspension in the macOS backend (T07, issue #33).
//!
//! These tests check the backend itself, below `Session::step_instruction` (covered in
//! `breakpoints.rs`), so they drive it through the session's escape hatch
//! (`d.session_mut().backend_mut()`): plant a trap with `write_memory`, run to it, put
//! the original bytes back, then resume with `ResumeRequest::step(thread)`
//! (that thread steps, every other thread stays suspended) and `wait`. Every test leaves
//! the backend *Stopped* or the process gone, so the session's drop works.
#![cfg(target_os = "macos")]

mod basic_step;
mod birth_policy;
mod helpers;
mod new_thread_hold;
mod real_step;
