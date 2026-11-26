use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use super::DEFAULT_TIMEOUT;
use super::context::TestContext;
use super::launch::launch_stalled_guidance;
use super::tracking::Shared;

/// Runs a test body under the [`DEFAULT_TIMEOUT`] deadline.
///
/// See [`run_with_timeout`].
///
/// # Panics
///
/// Panics (failing the test) when the body panics, when the deadline passes, or when a
/// launch hits a permission failure.
pub fn run<F>(body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    run_with_timeout(DEFAULT_TIMEOUT, body);
}

/// Runs a test body on a worker thread with a hard deadline.
///
/// The body receives a [`TestContext`] for launching fixtures. When the body returns, any
/// process it launched that is still alive is killed. When the body panics, its panic is
/// resumed on the calling thread with the original payload, so `#[should_panic]` and the
/// libtest failure message work as usual. When `timeout` passes first, every process launched
/// through this context is killed and the test fails with a message that says it timed out;
/// the worker thread is abandoned. The controller joins output readers and gives process
/// cleanup one shared settle window.
///
/// # Panics
///
/// Panics (failing the test) when the body panics, when the deadline passes, when a launch
/// hits a permission failure, or when the worker thread cannot be spawned.
pub fn run_with_timeout<F>(timeout: Duration, body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    let (events_tx, events_rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    let context = TestContext {
        shared: Arc::clone(&shared),
        events: events_tx.clone(),
    };

    // Name the worker after the test so panic messages point at the right test.
    let name = thread::current().name().unwrap_or("ferros-test").to_owned();
    let spawned = thread::Builder::new().name(name).spawn(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(|| body(&context)));
        drop(context);
        let _ = events_tx.send(Event::Finished(result));
    });
    if let Err(err) = spawned {
        panic!("ferros-tests: failed to spawn the test worker thread: {err}");
    }

    let deadline = Instant::now() + timeout;
    let outcome = match events_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Event::Finished(result)) => Outcome::Finished(result),
        Ok(Event::LaunchStalled) => Outcome::LaunchStalled,
        Err(RecvTimeoutError::Timeout) => Outcome::TimedOut,
        Err(RecvTimeoutError::Disconnected) => {
            Outcome::Finished(Err(Box::new("ferros-tests: the test worker thread vanished")))
        }
    };

    if !matches!(outcome, Outcome::Finished(_)) {
        shared.cancelled.store(true, Ordering::SeqCst);
    }
    let killed = shared.kill_all();
    shared.remove_scratch();

    match outcome {
        Outcome::Finished(Ok(())) => {}
        Outcome::Finished(Err(payload)) => panic::resume_unwind(payload),
        Outcome::LaunchStalled => panic!("{}", launch_stalled_guidance()),
        Outcome::TimedOut => {
            panic!("ferros-tests: test timed out after {timeout:?}; killed its debuggee processes {killed:?}")
        }
    }
}

pub(super) enum Event
{
    Finished(thread::Result<()>),
    LaunchStalled,
}

enum Outcome
{
    Finished(thread::Result<()>),
    LaunchStalled,
    TimedOut,
}
