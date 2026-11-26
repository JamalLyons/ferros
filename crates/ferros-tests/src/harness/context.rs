use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[cfg(doc)]
use ferros_core::Session;
use ferros_core::backend::StdioSpec;
use ferros_core::{DebuggerError, LaunchSpec, ProcessId};

use super::LAUNCH_DEADLINE;
use super::debuggee::{CaptureDir, Debuggee};
use super::launch::{LaunchWatchdog, launch_stalled_guidance, permission_guidance};
use super::process::{signal_tagged, unique_tag};
use super::run::Event;
use super::tracking::{ProcessCleanup, Shared, Tracked};
#[cfg(doc)]
use super::{TEST_ID_ARG_PREFIX, run};
use crate::{Fixture, scratch};

/// Handle passed to a test body by [`run`]; launches fixtures and tracks them for cleanup.
pub struct TestContext
{
    pub(super) shared: Arc<Shared>,
    pub(super) events: Sender<Event>,
}

impl TestContext
{
    /// Launches `fixture` with no extra arguments. See [`TestContext::launch_with_args`].
    ///
    /// # Panics
    ///
    /// Panics when the launch fails; see [`TestContext::launch_with_args`].
    #[must_use]
    pub fn launch(&self, fixture: &Fixture) -> Debuggee
    {
        self.launch_with_args(fixture, &[])
    }

    /// Launches `fixture` under a new [`Session`], suspended at entry, with `args` as its
    /// arguments (followed by the harness's identifying argument).
    ///
    /// The debuggee's stdout and stderr go to two files in a temporary directory the
    /// harness creates for this launch (they never reach the test's output); their contents
    /// are available through [`Debuggee::stdout`] and [`Debuggee::stderr`]. Its stdin is
    /// the null device.
    ///
    /// # Panics
    ///
    /// Panics when the launch fails. A denied `task_for_pid`, or a launch that does not return
    /// within [`LAUNCH_DEADLINE`], fails with guidance on signing the test executable. Any
    /// process left behind by the failed launch is killed first.
    #[must_use]
    pub fn launch_with_args(&self, fixture: &Fixture, args: &[&str]) -> Debuggee
    {
        match self.try_launch_with_args(fixture, args) {
            Ok(debuggee) => debuggee,
            Err(DebuggerError::PermissionDenied(detail)) => panic!("{}", permission_guidance(&detail)),
            Err(err) => panic!("ferros-tests: failed to launch fixture '{}': {err}", fixture.name()),
        }
    }

    /// Like [`TestContext::launch_with_args`], but returns launch errors instead of panicking.
    ///
    /// Whatever the result, no process from a failed launch survives this call.
    ///
    /// # Errors
    ///
    /// Returns the error from `MacOSBackend::new` or `Session::launch`, or an I/O error
    /// when the capture directory cannot be created.
    ///
    /// # Panics
    ///
    /// Panics when the test has already been failed by its deadline, when the fixture path
    /// is not valid UTF-8, or when the launch deadline passes (in that case the controlling
    /// thread fails the test with the permission guidance).
    pub fn try_launch_with_args(&self, fixture: &Fixture, args: &[&str]) -> Result<Debuggee, DebuggerError>
    {
        self.try_launch_with_backend(fixture, args, ferros_macos::MacOSBackend::new()?)
    }

    /// Like [`TestContext::launch_with_args`], but the session drives `backend`, which the
    /// test may have prepared first (for example to keep a test hook handle of it).
    ///
    /// # Panics
    ///
    /// As for [`TestContext::launch_with_args`].
    #[must_use]
    pub fn launch_with_backend(&self, fixture: &Fixture, args: &[&str], backend: ferros_macos::MacOSBackend) -> Debuggee
    {
        match self.try_launch_with_backend(fixture, args, backend) {
            Ok(debuggee) => debuggee,
            Err(DebuggerError::PermissionDenied(detail)) => panic!("{}", permission_guidance(&detail)),
            Err(err) => panic!("ferros-tests: failed to launch fixture '{}': {err}", fixture.name()),
        }
    }

    fn try_launch_with_backend(
        &self,
        fixture: &Fixture,
        args: &[&str],
        backend: ferros_macos::MacOSBackend,
    ) -> Result<Debuggee, DebuggerError>
    {
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to launch '{}': this test already failed its deadline",
            fixture.name()
        );

        let tag = unique_tag();
        let capture = CaptureDir::create(&tag)?;
        let spec = LaunchSpec::new(fixture.path())
            .args(args.iter().copied())
            .arg(tag.as_str())
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Path(capture.stdout_path()))
            .stderr(StdioSpec::Path(capture.stderr_path()));

        let child_cleanup = backend.child_cleanup();
        let mut session = ferros_macos::session_with(backend);

        let entry = self
            .shared
            .track(tag.clone(), ProcessCleanup::Launched(child_cleanup), Some(capture));

        // Watchdog: if launch has not returned by the deadline, it tells the controller (which
        // fails the test) and kills the half-launched process by its tag.
        let events = self.events.clone();
        let watchdog = LaunchWatchdog::arm(tag, LAUNCH_DEADLINE, move || {
            let _ = events.send(Event::LaunchStalled);
        });
        let launched = session.launch(&spec);
        if watchdog.disarm() {
            // The launch outlived its deadline and the test is being failed for it. Whatever
            // launch returned in the end, leave no process and report the same reason.
            signal_tagged(&entry.tag);
            drop(session);
            self.shared.finish(&entry);
            panic!("{}", launch_stalled_guidance());
        }

        let pid = match launched {
            Ok(pid) => pid,
            Err(err) => {
                // A failed launch can leave a suspended child behind (task_for_pid denied
                // after posix_spawn). Kill it by its argument before releasing the session.
                signal_tagged(&entry.tag);
                drop(session);
                self.shared.finish(&entry);
                return Err(err);
            }
        };

        let Some([stdout, stderr]) = entry.start_readers() else {
            // The controller closed the capture while launch was in flight.
            drop(session);
            self.shared.finish(&entry);
            panic!(
                "ferros-tests: launch of '{}' returned after this test already failed its deadline",
                fixture.name()
            );
        };
        let guard = Debuggee {
            session,
            pid,
            stdout,
            stderr,
            entry,
            shared: Arc::clone(&self.shared),
        };

        // The controller may have given up while launch was blocked; do not keep running.
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: launch of '{}' returned after this test already failed its deadline",
            fixture.name()
        );
        Ok(guard)
    }

    /// Starts `fixture` with `args` as an ordinary child process, not under the debugger
    /// (for example to attach to it later). The harness's identifying argument is
    /// appended, stdio is the null device, and the process is killed on the test's
    /// deadline like a launched one.
    ///
    /// # Panics
    ///
    /// Panics when the test has already failed its deadline or the process cannot be
    /// started.
    #[must_use]
    pub fn spawn(&self, fixture: &Fixture, args: &[&str]) -> Spawned
    {
        let entry = self
            .shared
            .track(unique_tag(), ProcessCleanup::Spawned(Mutex::default()), None);
        // Publish the handle under the same lock the controller uses to kill and reap.
        let mut slot = entry.spawned_child();
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: refusing to spawn '{}': this test already failed its deadline",
            fixture.name()
        );
        let child = Command::new(fixture.path())
            .args(args)
            .arg(&entry.tag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(err) => {
                self.shared.forget(&entry.tag);
                panic!("ferros-tests: failed to spawn fixture '{}': {err}", fixture.name());
            }
        };
        let pid = ProcessId::from(child.id());
        *slot = Some(child);
        drop(slot);
        let guard = Spawned {
            pid,
            entry,
            shared: Arc::clone(&self.shared),
        };
        assert!(
            !self.shared.cancelled.load(Ordering::SeqCst),
            "ferros-tests: spawn of '{}' returned after this test already failed its deadline",
            fixture.name()
        );
        guard
    }

    /// Registers a file or directory for removal when the test ends: on normal completion,
    /// on a panic in the body, and when the deadline fires (a deadline cannot unwind the
    /// abandoned worker, so a drop guard alone would leak it). Removal runs after the test's
    /// processes are killed and ignores a path that is already gone. Registering after the
    /// deadline fired removes the path immediately.
    pub fn remove_on_cleanup(&self, path: impl Into<PathBuf>)
    {
        self.shared.register_scratch(path.into());
    }

    /// Creates a unique empty file with [`scratch::create_file`] and registers it with
    /// [`TestContext::remove_on_cleanup`].
    ///
    /// # Panics
    ///
    /// Panics when the file cannot be created.
    #[must_use]
    pub fn scratch_file(&self, stem: &str, suffix: &str) -> (File, PathBuf)
    {
        let (file, path) = scratch::create_file(stem, suffix);
        self.remove_on_cleanup(path.clone());
        (file, path)
    }

    /// Reserves a fresh identifying argument (see [`TEST_ID_ARG_PREFIX`]) for processes the
    /// test starts some other way, for example through the `ferros` binary. Every process
    /// whose command line contains it is killed on the test's deadline, like a launched
    /// fixture, and when the returned [`Tag`] is dropped.
    ///
    /// # Panics
    ///
    /// Panics when the test has already failed its deadline.
    #[must_use]
    pub fn tag(&self) -> Tag
    {
        Tag {
            entry: self.shared.track(unique_tag(), ProcessCleanup::Tagged, None),
            shared: Arc::clone(&self.shared),
        }
    }
}

/// An identifying argument reserved by [`TestContext::tag`]. Dropping it kills every
/// process still carrying it.
pub struct Tag
{
    entry: Arc<Tracked>,
    shared: Arc<Shared>,
}

impl Tag
{
    /// The argument, `--ferros-test-id=...`, to pass to the processes it should cover.
    #[must_use]
    pub fn as_str(&self) -> &str
    {
        &self.entry.tag
    }
}

impl Drop for Tag
{
    fn drop(&mut self)
    {
        self.shared.finish(&self.entry);
    }
}

/// A fixture started by [`TestContext::spawn`], not under the debugger. Dropping it kills
/// and reaps the process.
pub struct Spawned
{
    pid: ProcessId,
    entry: Arc<Tracked>,
    shared: Arc<Shared>,
}

impl Spawned
{
    /// Process id of the fixture.
    #[must_use]
    pub fn pid(&self) -> ProcessId
    {
        self.pid
    }

    /// Check whether this fixture exited, collecting its status without blocking.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if querying the child status fails.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>
    {
        self.entry
            .spawned_child()
            .as_mut()
            .ok_or_else(|| std::io::Error::other("spawned child was not registered"))?
            .try_wait()
    }
}

impl Drop for Spawned
{
    fn drop(&mut self)
    {
        self.shared.finish(&self.entry);
    }
}
