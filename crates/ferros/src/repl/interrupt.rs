//! Ctrl-C handling.
//!
//! ferros never dies on `SIGINT`. The handler installed by [`install`] only sets a flag;
//! while the debuggee runs, [`wait_for_stop`] waits in short slices and turns a set flag
//! into [`Session::interrupt`], so Ctrl-C stops the debuggee and returns to the prompt.
//!
//! The debuggee is in ferros's process group, so a Ctrl-C typed at the terminal also
//! sends it `SIGINT` directly. That signal is reported as its own stop
//! (`stopped: signal 2`), possibly instead of `stopped: interrupted`, and `continue`
//! does not pass it on to the program.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ferros_core::{Result, Session, StopEvent};
use ferros_utils::warn;

/// Length of each `Session::wait` slice while the debuggee runs.
pub const WAIT_SLICE: Duration = Duration::from_millis(50);

/// Install the `SIGINT` handler and return the flag it sets.
///
/// If the handler cannot be installed (for example a second call in one process), a
/// warning is logged and the returned flag is simply never set.
pub fn install() -> Arc<AtomicBool>
{
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&flag);
    if let Err(error) = ctrlc::set_handler(move || handler_flag.store(true, Ordering::SeqCst)) {
        warn!("could not install the Ctrl-C handler: {error}");
    }
    flag
}

/// Wait until the running debuggee stops, interrupting it whenever `flag` is set.
///
/// # Errors
///
/// The error of [`Session::wait`].
pub fn wait_for_stop(session: &mut Session, flag: &AtomicBool) -> Result<StopEvent>
{
    loop {
        if flag.swap(false, Ordering::SeqCst)
            && let Err(error) = session.interrupt()
        {
            // The process may be exiting; its terminal event is still awaited below.
            warn!("interrupt failed: {error}");
        }
        if let Some(event) = session.wait(Some(WAIT_SLICE))? {
            return Ok(event);
        }
    }
}
