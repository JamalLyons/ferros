//! POSIX job control for the command-line front end (issue #122).
//!
//! A launched program runs in its own background process group, so the terminal's
//! job-control keys reach only the debugger. Two pieces live here because they need
//! `unsafe` libc calls, which the CLI crate forbids:
//!
//! - [`SuspendCatcher`]: while the program runs, Ctrl-Z (`SIGTSTP`) sets a flag instead
//!   of stopping the debugger, so the front end can interrupt the program first and then
//!   stop itself with [`suspend_process_group`].
//! - [`Foreground`]: hands the terminal foreground (`tcsetpgrp`) and the program's
//!   terminal settings to the program's process group while it runs, and takes both back
//!   at each stop.

use std::io;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

use ferros_core::ProcessId;

/// Set by [`on_sigtstp`]; read and cleared by [`SuspendCatcher`].
static SUSPEND_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The `SIGTSTP` handler: only an atomic store, which is async-signal-safe.
extern "C" fn on_sigtstp(_signal: libc::c_int)
{
    SUSPEND_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install `handler` for `signal` and return the previous action.
fn set_action(signal: libc::c_int, handler: libc::sighandler_t) -> io::Result<libc::sigaction>
{
    // SAFETY: a zeroed `sigaction` is a valid value; every field used is set below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    action.sa_flags = libc::SA_RESTART;
    let mut previous = MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: both pointers refer to live `sigaction` values; `sigemptyset` only writes
    // the mask, and `sigaction` fills `previous` on success.
    unsafe {
        libc::sigemptyset(&raw mut action.sa_mask);
        if libc::sigaction(signal, &raw const action, previous.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(previous.assume_init())
    }
}

/// While alive, `SIGTSTP` (terminal Ctrl-Z) sets a flag instead of stopping the process.
///
/// Dropping it restores the action that was installed before. Only one should be alive
/// at a time: the flag is process-wide.
pub struct SuspendCatcher
{
    previous: libc::sigaction,
}

impl SuspendCatcher
{
    /// Catch `SIGTSTP` from now on. A request from before this call is forgotten.
    ///
    /// # Errors
    ///
    /// The OS error of `sigaction`.
    pub fn install() -> io::Result<Self>
    {
        SUSPEND_REQUESTED.store(false, Ordering::SeqCst);
        let previous = set_action(libc::SIGTSTP, on_sigtstp as *const () as libc::sighandler_t)?;
        Ok(Self { previous })
    }

    /// Whether `SIGTSTP` arrived since [`install`](Self::install). The flag stays set.
    #[must_use]
    pub fn requested(&self) -> bool
    {
        SUSPEND_REQUESTED.load(Ordering::SeqCst)
    }
}

impl Drop for SuspendCatcher
{
    fn drop(&mut self)
    {
        // SAFETY: `previous` was filled by `sigaction` in `install`.
        unsafe {
            libc::sigaction(libc::SIGTSTP, &raw const self.previous, std::ptr::null_mut());
        }
    }
}

/// Stop the caller's process group, as terminal Ctrl-Z would; returns once the group is
/// continued (`fg`, or `SIGCONT`).
///
/// The signal is `SIGSTOP`, not `SIGTSTP`: the kernel discards a `SIGTSTP` with the
/// default action sent to an orphaned process group (one whose session leader started
/// it directly, as a terminal emulator or a pty test does), while `SIGSTOP` always stops.
/// The shell sees the same stopped job either way.
///
/// # Errors
///
/// The OS error of `kill`.
pub fn suspend_process_group() -> io::Result<()>
{
    // SAFETY: `kill` takes plain integers; pid 0 names the caller's process group.
    if unsafe { libc::kill(0, libc::SIGSTOP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Run `call` with `SIGTTOU` blocked in this thread: a process in a background group may
/// then change the terminal's foreground group and settings instead of being stopped.
fn without_sigttou<T>(call: impl FnOnce() -> T) -> T
{
    let mut blocked = MaybeUninit::<libc::sigset_t>::uninit();
    let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: the sets are initialised by `sigemptyset` before use, and
    // `pthread_sigmask` fills `previous` before it is restored.
    unsafe {
        libc::sigemptyset(blocked.as_mut_ptr());
        libc::sigaddset(blocked.as_mut_ptr(), libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, blocked.as_ptr(), previous.as_mut_ptr());
    }
    let result = call();
    // SAFETY: `previous` was filled by the first `pthread_sigmask` call.
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, previous.as_ptr(), std::ptr::null_mut());
    }
    result
}

/// Read the settings of terminal `fd`.
fn get_attributes(fd: libc::c_int) -> io::Result<libc::termios>
{
    let mut attributes = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` fills `attributes` on success.
    unsafe {
        if libc::tcgetattr(fd, attributes.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes.assume_init())
    }
}

/// Apply `attributes` to terminal `fd` once pending output is written.
fn set_attributes(fd: libc::c_int, attributes: &libc::termios) -> io::Result<()>
{
    // SAFETY: `attributes` is a valid `termios` read by `tcgetattr`.
    if without_sigttou(|| unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, attributes) }) == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Make `group` the foreground process group of terminal `fd`.
fn set_foreground(fd: libc::c_int, group: libc::pid_t) -> io::Result<()>
{
    // SAFETY: `tcsetpgrp` takes plain integers.
    if without_sigttou(|| unsafe { libc::tcsetpgrp(fd, group) }) == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The controlling terminal on stdin, shared between the debugger and one program.
///
/// [`give`](Self::give) makes the program's group the foreground group and restores the
/// settings the program left at its last stop; [`take_back`](Self::take_back) saves the
/// program's settings and restores the debugger's group and the settings it had when
/// this value was created.
pub struct Foreground
{
    fd: libc::c_int,
    own_group: libc::pid_t,
    own_attributes: libc::termios,
    program_attributes: Option<libc::termios>,
}

impl Foreground
{
    /// The terminal on stdin, whose foreground group must be the caller's.
    ///
    /// # Errors
    ///
    /// When stdin is not a terminal, or the caller is not its foreground group.
    pub fn new() -> io::Result<Self>
    {
        let fd = libc::STDIN_FILENO;
        let own_attributes = get_attributes(fd)?;
        // SAFETY: `getpgrp` and `tcgetpgrp` take and return plain integers.
        let (own_group, foreground) = unsafe { (libc::getpgrp(), libc::tcgetpgrp(fd)) };
        if foreground != own_group {
            return Err(io::Error::other("ferros is not the terminal's foreground job"));
        }
        Ok(Self {
            fd,
            own_group,
            own_attributes,
            program_attributes: None,
        })
    }

    /// Hand the terminal to `group`, with the settings the program had at its last stop.
    ///
    /// # Errors
    ///
    /// The OS error of `tcsetattr` or `tcsetpgrp`.
    pub fn give(&mut self, group: ProcessId) -> io::Result<()>
    {
        let group = libc::pid_t::try_from(group.0).map_err(|_| io::Error::other("process group out of range"))?;
        if let Some(attributes) = &self.program_attributes {
            set_attributes(self.fd, attributes)?;
        }
        set_foreground(self.fd, group)
    }

    /// Take the terminal back for the debugger, saving the program's settings.
    ///
    /// # Errors
    ///
    /// The OS error of `tcsetpgrp`, `tcgetattr` or `tcsetattr`.
    pub fn take_back(&mut self) -> io::Result<()>
    {
        set_foreground(self.fd, self.own_group)?;
        self.program_attributes = Some(get_attributes(self.fd)?);
        set_attributes(self.fd, &self.own_attributes)
    }
}
