//! Harness for integration tests that debug real fixture processes (macOS only).
//!
//! Every integration test body runs under [`run`] (or [`run_with_timeout`]), which gives it a
//! [`TestContext`]. The context launches fixtures under a fresh [`Session`] (driving a
//! `ferros_macos::MacOSBackend`) and returns a [`Debuggee`] guard that owns the session and
//! kills the debuggee when dropped.
//!
//! ```ignore
//! use ferros_core::SessionState;
//! use ferros_tests::{fixture, harness};
//!
//! harness::run(|t| {
//!     let mut d = t.launch(&fixture!("hello"));
//!     assert_eq!(d.state(), SessionState::Stopped);
//!     d.resume().expect("resume");
//!     assert!(d.wait_for_stdout("hello from ferros fixture", std::time::Duration::from_secs(10)));
//! });
//! ```
//!
//! Guarantees:
//!
//! - **No leaked processes.** Each launch appends a unique `--ferros-test-id=...` argument to
//!   the fixture's argv. Dropping the [`Debuggee`] (normal return or unwinding) drops its
//!   session first, then sweeps processes by that argument; a failed launch kills any
//!   process left behind by the argument; a timeout kills every process the test launched.
//!   Cleanup also reaps owned children, joins output readers and removes capture directories.
//! - **Hard deadline.** The body runs on a worker thread. If it does not finish in time the
//!   test fails with a "timed out" message after its debuggees are killed. Only that test's
//!   processes are touched, so tests can run in parallel.
//! - **Fast permission failures.** If `task_for_pid` is denied, or `Session::launch` does not
//!   return within [`LAUNCH_DEADLINE`] (for example because the system is waiting on an
//!   authorization dialog), the test fails with guidance on signing the test executable.
//! - **Panics stay panics.** A panic in the body reaches libtest with its original payload.
//!
//! # Map of this module
//!
//! - `run`: [`run`], [`run_with_timeout`] and the controller loop that enforces the deadline.
//! - `tracking`: per-test bookkeeping of launched processes, capture and scratch paths, and
//!   the cleanup that kills and reaps them.
//! - `context`: [`TestContext`] (launching and spawning fixtures), [`Tag`] and [`Spawned`].
//! - `debuggee`: [`Debuggee`], the capture directory, and the output tail readers.
//! - `launch`: the launch watchdog and the permission and stall failure guidance.
//! - `process`: tag-based process discovery, killing and exit checks (`ps`, `kill`).
//! - `tests`: self-tests of the harness.

#[cfg(doc)]
use ferros_core::Session;

mod context;
mod debuggee;
mod launch;
mod process;
mod run;
mod tracking;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use self::context::{Spawned, Tag, TestContext};
pub use self::debuggee::Debuggee;
pub use self::process::{kill_tagged, process_is_gone, tagged_pids, wait_until_gone};
pub use self::run::{run, run_with_timeout};

/// Deadline applied by [`run`] to a whole test body.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`TestContext::launch`] waits for `Session::launch` to return before it treats
/// the launch as blocked on a permission prompt and fails the test.
pub const LAUNCH_DEADLINE: Duration = Duration::from_secs(5);

/// Prefix of the identifying argument appended as the last argv entry of every launched
/// fixture. `pgrep -f -- --ferros-test-id=` lists fixtures started by this harness.
pub const TEST_ID_ARG_PREFIX: &str = "--ferros-test-id=";

/// Entitlement the test executable needs to call `task_for_pid` without root.
pub const DEBUGGER_ENTITLEMENT: &str = "com.apple.security.cs.debugger";
