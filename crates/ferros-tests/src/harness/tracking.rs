use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use std::{fs, thread};

use super::debuggee::{Capture, CaptureDir, OutputBuffer};
use super::process::{KILL_SETTLE, signal_tagged, wait_for_killed};

pub(super) struct Tracked
{
    pub(super) tag: String,
    pub(super) process: ProcessCleanup,
    pub(super) capture: Mutex<Option<Capture>>,
}

pub(super) enum ProcessCleanup
{
    Tagged,
    Spawned(Mutex<Option<Child>>),
    Launched(ferros_macos::launch::ChildCleanup),
}

impl Tracked
{
    pub(super) fn spawned_child(&self) -> MutexGuard<'_, Option<Child>>
    {
        let ProcessCleanup::Spawned(child) = &self.process else {
            unreachable!("only a spawned fixture has a Child handle");
        };
        child.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn start_readers(&self) -> Option<[OutputBuffer; 2]>
    {
        let mut capture = self.capture.lock().unwrap_or_else(PoisonError::into_inner);
        let capture = capture.as_mut()?;
        let (stdout, stdout_reader) = OutputBuffer::spawn_reader(capture.dir.stdout_path());
        let (stderr, stderr_reader) = OutputBuffer::spawn_reader(capture.dir.stderr_path());
        capture.stdout = Some(stdout_reader);
        capture.stderr = Some(stderr_reader);
        Some([stdout, stderr])
    }

    pub(super) fn close_capture(&self)
    {
        let mut capture = self.capture.lock().unwrap_or_else(PoisonError::into_inner);
        // Keep the lock through the joins so another cleanup cannot return early.
        if let Some(Capture { dir, stdout, stderr }) = capture.take() {
            drop(stdout);
            drop(stderr);
            drop(dir);
        }
    }

    pub(super) fn kill_spawned(&self, deadline: Instant) -> Option<u32>
    {
        let mut slot = self.spawned_child();
        let child = slot.as_mut()?;
        let pid = child.id();
        let killed = matches!(child.try_wait(), Ok(None)) && child.kill().is_ok();
        drop(slot);

        // Keep try_wait available to the guard while cleanup polls for exit.
        while matches!(self.spawned_child().as_mut().map(Child::try_wait), Some(Ok(None))) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                eprintln!("ferros-tests: timed out reaping spawned child {pid}");
                break;
            }
            thread::sleep(Duration::from_millis(5).min(remaining));
        }
        killed.then_some(pid)
    }

    pub(super) fn clean_process(&self, deadline: Instant) -> Vec<u32>
    {
        let mut killed = Vec::new();
        if let ProcessCleanup::Spawned(_) = &self.process {
            killed.extend(self.kill_spawned(deadline));
        }
        let tagged = signal_tagged(&self.tag);
        // Traced children are collected by the backend's exit watcher. Detach transfers
        // the Unix parent's claim to this handle, so collect it before checking the table.
        if let ProcessCleanup::Launched(child) = &self.process
            && let Err(err) = child.reap()
        {
            eprintln!("ferros-tests: detached child cleanup failed: {err}");
        }
        wait_for_killed(&self.tag, &tagged, deadline);
        killed.extend(tagged);
        killed
    }
}

/// State shared between a test's worker thread, its guards and the controlling thread.
#[derive(Default)]
pub(super) struct Shared
{
    pub(super) tracked: Mutex<Vec<Arc<Tracked>>>,
    /// Scratch paths removed once processes are cleaned up, on every exit path.
    pub(super) scratch: Mutex<Vec<PathBuf>>,
    /// Set when the controller gave up on the test; later launches are refused.
    pub(super) cancelled: AtomicBool,
}

impl Shared
{
    pub(super) fn tracked(&self) -> MutexGuard<'_, Vec<Arc<Tracked>>>
    {
        self.tracked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn track(&self, tag: String, process: ProcessCleanup, dir: Option<CaptureDir>) -> Arc<Tracked>
    {
        let entry = Arc::new(Tracked {
            tag,
            process,
            capture: Mutex::new(dir.map(|dir| Capture {
                dir,
                stdout: None,
                stderr: None,
            })),
        });
        let mut tracked = self.tracked();
        assert!(
            !self.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to track a process after this test failed its deadline"
        );
        tracked.push(Arc::clone(&entry));
        entry
    }

    /// Registers `path` for removal; removes it at once if the test already failed its
    /// deadline, since the final removal pass may have run.
    pub(super) fn register_scratch(&self, path: PathBuf)
    {
        let mut scratch = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        if self.cancelled.load(Ordering::SeqCst) {
            drop(scratch);
            remove_scratch_path(&path);
        } else {
            scratch.push(path);
        }
    }

    /// Removes every registered scratch path. Runs after processes are killed so nothing
    /// still holds or recreates them.
    pub(super) fn remove_scratch(&self)
    {
        let paths = std::mem::take(&mut *self.scratch.lock().unwrap_or_else(PoisonError::into_inner));
        for path in paths {
            remove_scratch_path(&path);
        }
    }

    pub(super) fn forget(&self, tag: &str)
    {
        self.tracked().retain(|entry| entry.tag != tag);
    }

    pub(super) fn finish(&self, entry: &Tracked)
    {
        entry.clean_process(Instant::now() + KILL_SETTLE);
        entry.close_capture();
        self.forget(&entry.tag);
    }

    /// Stop capture synchronously. Give all process cleanups one shared settle window
    /// so an unreapable process cannot delay cleanup of another entry.
    pub(super) fn kill_all(&self) -> Vec<u32>
    {
        let entries = std::mem::take(&mut *self.tracked());
        let deadline = Instant::now() + KILL_SETTLE;
        let (done, results) = mpsc::channel();
        for entry in &entries {
            let entry = Arc::clone(entry);
            let done = done.clone();
            if let Err(err) = thread::Builder::new()
                .name("ferros-process-cleanup".to_owned())
                .spawn(move || {
                    let _ = done.send(entry.clean_process(deadline));
                })
            {
                eprintln!("ferros-tests: could not start process cleanup: {err}");
            }
        }
        drop(done);
        for entry in entries {
            entry.close_capture();
        }
        let mut killed = Vec::new();
        loop {
            match results.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(pids) => killed.extend(pids),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("ferros-tests: process cleanup did not finish within {KILL_SETTLE:?}");
                    break;
                }
            }
        }
        killed.sort_unstable();
        killed.dedup();
        killed
    }
}

/// Removes a file or directory tree, ignoring a path that is already gone.
fn remove_scratch_path(path: &Path)
{
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => return,
    };
    if let Err(err) = result {
        eprintln!("ferros-tests: could not remove scratch path {}: {err}", path.display());
    }
}
