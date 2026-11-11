//! Data types passed across the [`Backend`](super::Backend) boundary.
//!
//! None of these types contain an operating-system handle; they describe
//! launches, resumes, stops and loaded images in portable terms.

use std::path::PathBuf;

use crate::types::{Address, ThreadId};

/// Why a running target stopped, as reported by [`Backend::wait`](super::Backend::wait).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopEvent
{
    /// A thread executed a software trap instruction.
    ///
    /// `pc` is the raw program counter the operating system reported, not adjusted.
    /// The engine maps it to the trap address with
    /// [`Arch::pc_after_trap`](crate::arch::Arch::pc_after_trap).
    Breakpoint
    {
        /// Thread that executed the trap.
        thread: ThreadId,
        /// Program counter exactly as reported by the operating system.
        pc: Address,
    },

    /// A single-step requested with [`ThreadAction::Step`] completed.
    SingleStep
    {
        /// Thread that completed the step.
        thread: ThreadId,
    },

    /// A signal is about to be delivered to the target, which is stopped.
    ///
    /// The engine decides whether to pass it on through
    /// [`ResumeRequest::signal`] in the next [`Backend::resume`](super::Backend::resume).
    Signal
    {
        /// Thread the signal is directed at, if the OS reports one.
        thread: Option<ThreadId>,
        /// Signal number.
        signal: i32,
    },

    /// The target stopped because [`Backend::interrupt`](super::Backend::interrupt) was called.
    Interrupted
    {
        /// Thread chosen to report the stop, if any.
        thread: Option<ThreadId>,
    },

    /// The process exited normally. Terminal: the backend is back in the no-process state.
    Exited
    {
        /// Exit status.
        code: i32,
    },

    /// The process was terminated by a signal. Terminal: the backend is back in the
    /// no-process state.
    Killed
    {
        /// Signal that terminated the process.
        signal: i32,
    },

    /// Any other stop, carrying the operating system's raw reason as text.
    Other
    {
        /// Thread the stop relates to, if any.
        thread: Option<ThreadId>,
        /// Raw OS reason, for display and diagnostics.
        reason: String,
    },
}

impl StopEvent
{
    /// Thread associated with this stop, if any.
    #[must_use]
    pub fn thread(&self) -> Option<ThreadId>
    {
        match self {
            Self::Breakpoint { thread, .. } | Self::SingleStep { thread } => Some(*thread),
            Self::Signal { thread, .. } | Self::Interrupted { thread } | Self::Other { thread, .. } => *thread,
            Self::Exited { .. } | Self::Killed { .. } => None,
        }
    }

    /// Whether the process no longer exists after this event ([`Exited`](Self::Exited)
    /// or [`Killed`](Self::Killed)).
    #[must_use]
    pub fn is_terminal(&self) -> bool
    {
        matches!(self, Self::Exited { .. } | Self::Killed { .. })
    }
}

/// Where one standard stream of a launched program is connected.
///
/// To capture output, the caller creates a FIFO or pseudo-terminal it owns and passes
/// its path as [`StdioSpec::Path`]; no handle type ever appears in the interface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StdioSpec
{
    /// Share the debugger's own stream.
    #[default]
    Inherit,
    /// Connect the stream to the null device.
    Null,
    /// Open the file, FIFO or pty at this path for the stream.
    Path(PathBuf),
}

/// Environment of a launched program.
///
/// The program starts with the debugger's environment if `inherit` is true, or an
/// empty one otherwise; `vars` are then applied in order as additions or overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment
{
    /// Start from the debugger's environment (`true`) or an empty one (`false`).
    pub inherit: bool,
    /// Variables to set, applied in order after the base environment.
    pub vars: Vec<(String, String)>,
}

impl Default for Environment
{
    fn default() -> Self
    {
        Self {
            inherit: true,
            vars: Vec::new(),
        }
    }
}

/// Description of a program to start with [`Backend::launch`](super::Backend::launch).
///
/// ```
/// use ferros_core::backend::{LaunchSpec, StdioSpec};
///
/// let spec = LaunchSpec::new("/bin/echo")
///     .arg("hello")
///     .env("LANG", "C")
///     .stdout(StdioSpec::Null);
/// assert_eq!(spec.args, vec!["hello".to_string()]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec
{
    /// Path of the program to execute.
    pub program: PathBuf,
    /// Arguments, excluding argv\[0\] (the backend passes `program` as argv\[0\]).
    pub args: Vec<String>,
    /// Environment of the new process.
    pub env: Environment,
    /// Working directory; `None` uses the debugger's current directory.
    pub cwd: Option<PathBuf>,
    /// Standard input.
    pub stdin: StdioSpec,
    /// Standard output.
    pub stdout: StdioSpec,
    /// Standard error.
    pub stderr: StdioSpec,
}

impl LaunchSpec
{
    /// Launch `program` with no arguments, the inherited environment and directory,
    /// and inherited standard streams.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self
    {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Environment::default(),
            cwd: None,
            stdin: StdioSpec::Inherit,
            stdout: StdioSpec::Inherit,
            stderr: StdioSpec::Inherit,
        }
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self
    {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Replace the whole environment description.
    #[must_use]
    pub fn environment(mut self, env: Environment) -> Self
    {
        self.env = env;
        self
    }

    /// Choose whether the debugger's environment is inherited.
    #[must_use]
    pub fn inherit_env(mut self, inherit: bool) -> Self
    {
        self.env.inherit = inherit;
        self
    }

    /// Add or override one environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self
    {
        self.env.vars.push((key.into(), value.into()));
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self
    {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set standard input.
    #[must_use]
    pub fn stdin(mut self, stdin: StdioSpec) -> Self
    {
        self.stdin = stdin;
        self
    }

    /// Set standard output.
    #[must_use]
    pub fn stdout(mut self, stdout: StdioSpec) -> Self
    {
        self.stdout = stdout;
        self
    }

    /// Set standard error.
    #[must_use]
    pub fn stderr(mut self, stderr: StdioSpec) -> Self
    {
        self.stderr = stderr;
        self
    }
}

/// What one thread does when the target is resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadAction
{
    /// Run freely.
    Continue,
    /// Execute exactly one instruction, then stop with [`StopEvent::SingleStep`].
    Step,
    /// Stay suspended.
    Suspend,
}

/// Description of a resume, modelled on the GDB remote protocol's `vCont` packet.
///
/// Each thread runs [`action_for`](Self::action_for) its id: the last matching entry of
/// `thread_actions`, or `default_action` if none matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest
{
    /// Action of every thread not named in `thread_actions`.
    pub default_action: ThreadAction,
    /// Per-thread overrides; when a thread appears more than once, the last entry wins.
    pub thread_actions: Vec<(ThreadId, ThreadAction)>,
    /// Signal to deliver on resume to the thread that reported the last
    /// [`StopEvent::Signal`] (process-wide if that stop named no thread).
    /// `None` suppresses the pending signal.
    pub signal: Option<i32>,
}

impl ResumeRequest
{
    /// Every thread performs `action`; no signal is delivered.
    #[must_use]
    pub fn all(action: ThreadAction) -> Self
    {
        Self {
            default_action: action,
            thread_actions: Vec::new(),
            signal: None,
        }
    }

    /// Every thread continues; no signal is delivered.
    #[must_use]
    pub fn continue_all() -> Self
    {
        Self::all(ThreadAction::Continue)
    }

    /// `thread` single-steps while every other thread stays suspended.
    #[must_use]
    pub fn step(thread: ThreadId) -> Self
    {
        Self::all(ThreadAction::Suspend).with_thread(thread, ThreadAction::Step)
    }

    /// Add a per-thread override (later overrides win).
    #[must_use]
    pub fn with_thread(mut self, thread: ThreadId, action: ThreadAction) -> Self
    {
        self.thread_actions.push((thread, action));
        self
    }

    /// Deliver `signal` on resume.
    #[must_use]
    pub fn with_signal(mut self, signal: i32) -> Self
    {
        self.signal = Some(signal);
        self
    }

    /// Action `thread` performs: the last matching override, else the default.
    #[must_use]
    pub fn action_for(&self, thread: ThreadId) -> ThreadAction
    {
        self.thread_actions
            .iter()
            .rev()
            .find(|(id, _)| *id == thread)
            .map_or(self.default_action, |(_, action)| *action)
    }
}

/// An executable image (main program or library) mapped into the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage
{
    /// Path of the image file.
    pub path: PathBuf,
    /// Runtime address where the image is loaded.
    pub load_address: Address,
}
