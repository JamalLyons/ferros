use std::mem::DropGuard;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{panic, thread};

use super::debuggee::CaptureDir;
use super::launch::{LaunchWatchdog, permission_guidance};
use super::process::{KILL_SETTLE, pids_with_tag, tagged_pids, unique_tag};
use super::tracking::ProcessCleanup;
use super::{DEBUGGER_ENTITLEMENT, run_with_timeout};

/// A long-running stand-in for a half-launched fixture: `yes` repeats its argument forever.
fn spawn_tagged(tag: &str) -> DropGuard<Child, impl FnOnce(Child) + use<>>
{
    let child = Command::new("/usr/bin/yes")
        .arg(tag)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /usr/bin/yes");
    DropGuard::new(child, |mut child| {
        let _ = child.kill();
        let _ = child.wait();
    })
}

#[test]
fn deadline_joins_capture_readers()
{
    let tag = unique_tag();
    let dir = CaptureDir::create(&tag).expect("create capture");
    let path = dir.path.clone();
    let (buffers_tx, buffers_rx) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let result = panic::catch_unwind(|| {
        run_with_timeout(Duration::from_secs(2), move |t| {
            let entry = t.shared.track(tag, ProcessCleanup::Tagged, Some(dir));
            let buffers = entry.start_readers().expect("start readers");
            for buffer in &buffers {
                assert_eq!(Arc::strong_count(&buffer.bytes), 2, "reader must be live before timeout");
            }
            buffers_tx.send(buffers).expect("send buffers");
            let _ = released.recv();
            drop(entry);
        });
    });

    assert!(result.is_err(), "a body that outlives its deadline must fail");
    for buffer in buffers_rx.try_recv().expect("receive buffers") {
        assert_eq!(Arc::strong_count(&buffer.bytes), 1, "reader kept polling after timeout");
    }
    assert!(!path.exists(), "the controller must remove the capture directory");
    drop(release);
}

#[test]
fn deadline_uses_one_settle_window_for_all_tags()
{
    let (children_tx, children_rx) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let timeout = Duration::from_secs(2);
    let started = Instant::now();
    let result = panic::catch_unwind(|| {
        run_with_timeout(timeout, move |t| {
            let tags = [t.tag(), t.tag(), t.tag()];
            // Keep the Child handles outside the harness. Each killed process will
            // remain a zombie for the entire settle window unless this test reaps it.
            let children = tags.each_ref().map(|tag| spawn_tagged(tag.as_str()));
            children_tx.send(children).expect("send children");
            let _ = released.recv();
            drop(tags);
        });
    });
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a body that outlives its deadline must fail");
    for mut child in children_rx.try_recv().expect("receive children") {
        let status = child.try_wait().expect("try_wait").expect("tagged child was not killed");
        assert!(!status.success(), "tagged child should have been killed");
    }
    drop(release);
    assert!(
        elapsed < timeout + KILL_SETTLE + Duration::from_secs(2),
        "timeout cleanup took {elapsed:?} including the {timeout:?} deadline"
    );
}

#[test]
fn stalled_launch_watchdog_reports_and_kills_the_tagged_process()
{
    let tag = unique_tag();
    let mut child = spawn_tagged(&tag);
    let (stalled_tx, stalled_rx) = mpsc::channel();

    let watchdog = LaunchWatchdog::arm(tag.clone(), Duration::from_millis(200), move || {
        let _ = stalled_tx.send(());
    });

    stalled_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the watchdog never reported the stall");
    // No controller is involved here: the watchdog itself must end the process.
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "the watchdog did not kill the tagged process");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success(), "the tagged process should have been killed, got {status}");
    assert!(watchdog.disarm(), "disarm() must report that the launch stalled");
    assert_eq!(tagged_pids(&tag), Vec::<u32>::new());
}

#[test]
fn concurrent_tag_searches_do_not_list_each_other()
{
    // Nothing carries this tag, so every search must come back empty even while other
    // threads search for the same tag (the watchdog does exactly that during a sweep).
    let tag = unique_tag();
    let searchers: Vec<_> = (0..4)
        .map(|_| {
            let tag = tag.clone();
            thread::spawn(move || (0..10).flat_map(|_| tagged_pids(&tag)).collect::<Vec<u32>>())
        })
        .collect();
    for searcher in searchers {
        assert_eq!(searcher.join().expect("searcher thread"), Vec::<u32>::new());
    }
}

#[test]
fn pids_with_tag_matches_only_command_lines_that_contain_the_tag()
{
    let ps_output = "    1 /sbin/launchd\n  412 /tmp/ferros-fixture-hello --ferros-test-id=7-0-1\n 5021 /usr/bin/yes \
                     --ferros-test-id=7-1-2\n  733 (yes)\n";
    assert_eq!(pids_with_tag(ps_output, "--ferros-test-id=7-0-1"), vec![412]);
    assert_eq!(pids_with_tag(ps_output, "--ferros-test-id=7-"), vec![412, 5021]);
    assert_eq!(pids_with_tag(ps_output, "--ferros-test-id=9"), Vec::<u32>::new());
}

#[test]
fn disarmed_watchdog_stays_quiet()
{
    let tag = unique_tag();
    let mut child = spawn_tagged(&tag);
    let (stalled_tx, stalled_rx) = mpsc::channel();

    let watchdog = LaunchWatchdog::arm(tag.clone(), Duration::from_millis(300), move || {
        let _ = stalled_tx.send(());
    });
    assert!(!watchdog.disarm(), "disarm() before the deadline must not report a stall");

    thread::sleep(Duration::from_millis(600));
    assert!(stalled_rx.try_recv().is_err(), "a disarmed watchdog reported a stall");
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "a disarmed watchdog killed the process"
    );
}

#[test]
fn permission_guidance_keeps_all_detail_lines()
{
    let detail = "task_for_pid() failed with KERN_FAILURE\n\nRetry with a signed debugger.";
    let message = permission_guidance(detail);
    assert!(message.ends_with(detail));
    assert!(message.contains(DEBUGGER_ENTITLEMENT));
}
