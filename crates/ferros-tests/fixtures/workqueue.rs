//! Runs two work items on a GCD global queue, one after the other.
//!
//! The first item starts a workqueue thread at `libsystem_pthread`'s `_start_wqthread`.
//! That thread then parks in the kernel. The second item, submitted from
//! `submit_second`, is normally handed to the same parked thread, which enters
//! `_start_wqthread` again. The second item prints whether it ran on the first one's
//! thread, so a test can tell a reused thread from a new one.
#[cfg(target_os = "macos")]
use std::sync::mpsc;
#[cfg(target_os = "macos")]
use std::thread::{self, ThreadId};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};

#[cfg(target_os = "macos")]
fn queue() -> DispatchRetained<DispatchQueue>
{
    DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Default))
}

#[cfg(target_os = "macos")]
#[inline(never)]
fn submit_second(first: ThreadId)
{
    let (done, finished) = mpsc::channel();
    queue().exec_async(move || {
        let same = thread::current().id() == first;
        println!("second block ran, same thread: {same}");
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(30))
        .expect("the second work item did not run");
}

#[cfg(target_os = "macos")]
fn main()
{
    let (ran, first) = mpsc::channel();
    queue().exec_async(move || {
        println!("first block ran");
        let _ = ran.send(thread::current().id());
    });
    let first = first.recv().expect("the first work item did not run");
    // Long enough for the workqueue thread to return to the kernel and park.
    thread::sleep(Duration::from_millis(200));
    submit_second(first);
    println!("workqueue finished");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
