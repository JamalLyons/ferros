//! # macOS Process Launch
//!
//! Process launching using `posix_spawn` with suspended start.
//!
//! This module handles launching processes under debugger control using
//! `posix_spawn()` with the `POSIX_SPAWN_START_SUSPENDED` flag. This allows
//! the debugger to set breakpoints before the process begins execution.
//!
//! ## Mach APIs Used
//!
//! - **posix_spawn()**: Launch a new process
//! - **posix_spawnattr_setflags()**: Set spawn attributes (START_SUSPENDED)
//! - **posix_spawn_file_actions_addopen()**: Connect stdin/stdout/stderr to a path or `/dev/null`
//! - **posix_spawn_file_actions_addchdir_np()**: Set the working directory
//!
//! ## References
//!
//! - [posix_spawn(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawn.3.html)

use std::ffi::{CString, OsString};
use std::io::{Error, ErrorKind};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::ptr;

use ferros_core::backend::{Environment, LaunchSpec, StdioSpec};
use ferros_core::error::{DebuggerError, Result};
use libc::c_int;

use crate::ffi;

/// Process launch functions for the macOS backend.
pub(crate) struct LaunchManager;

/// Owns an initialized `posix_spawnattr_t` and destroys it on drop.
struct SpawnAttr(libc::posix_spawnattr_t);

impl Drop for SpawnAttr
{
    fn drop(&mut self)
    {
        unsafe {
            let _ = ffi::posix_spawnattr_destroy(&mut self.0);
        }
    }
}

/// Owns an initialized `posix_spawn_file_actions_t` and destroys it on drop.
struct FileActions(libc::posix_spawn_file_actions_t);

impl Drop for FileActions
{
    fn drop(&mut self)
    {
        unsafe {
            let _ = libc::posix_spawn_file_actions_destroy(&mut self.0);
        }
    }
}

fn c_string(bytes: &[u8], what: &str) -> Result<CString>
{
    CString::new(bytes).map_err(|e| DebuggerError::InvalidArgument(format!("Invalid {what}: {e}")))
}

/// Turn a failed spawn-setup call (an errno value) into an error that keeps its kind.
fn spawn_error(desc: &str, errno: c_int) -> DebuggerError
{
    let err = Error::from_raw_os_error(errno);
    DebuggerError::Io(Error::new(err.kind(), format!("Failed to {desc}: {err}")))
}

impl LaunchManager
{
    /// Build the environment of the new process from `env`.
    ///
    /// Starts from the debugger's environment when `env.inherit` is true, or an empty
    /// one otherwise, then applies `env.vars` in order (a later entry overrides an
    /// earlier one with the same key). Returns `KEY=VALUE` strings for `posix_spawn`.
    fn build_environment(env: &Environment) -> Result<Vec<CString>>
    {
        let mut vars: Vec<(OsString, OsString)> = if env.inherit {
            std::env::vars_os().collect()
        } else {
            Vec::new()
        };
        for (key, value) in &env.vars {
            let key = OsString::from(key);
            let value = OsString::from(value);
            match vars.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = value,
                None => vars.push((key, value)),
            }
        }

        vars.into_iter()
            .map(|(key, value)| {
                let mut entry = key.into_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                c_string(&entry, "environment variable")
            })
            .collect()
    }

    /// Add the file action that connects `fd` as described by `spec`.
    ///
    /// `Inherit` adds nothing. `Null` opens `/dev/null`. `Path` opens the path, for
    /// writing (created if missing, appended to) on stdout and stderr and for reading on
    /// stdin.
    fn add_stdio(actions: &mut FileActions, fd: c_int, spec: &StdioSpec, name: &str) -> Result<()>
    {
        let path: &Path = match spec {
            StdioSpec::Inherit => return Ok(()),
            StdioSpec::Null => Path::new("/dev/null"),
            StdioSpec::Path(path) => path,
        };
        let flags = if fd == libc::STDIN_FILENO {
            libc::O_RDONLY
        } else {
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND
        };
        let path = c_string(path.as_os_str().as_bytes(), "stdio path")?;
        let result = unsafe { libc::posix_spawn_file_actions_addopen(&mut actions.0, fd, path.as_ptr(), flags, 0o644) };
        if result != 0 {
            return Err(spawn_error(&format!("connect {name}"), result));
        }
        Ok(())
    }

    /// Launch a process under debugger control.
    ///
    /// This spawns `spec.program` using `posix_spawn()` with the
    /// `POSIX_SPAWN_START_SUSPENDED` flag, allowing breakpoints to be set
    /// before execution begins. `spec.program` is passed as argv\[0\], followed by
    /// `spec.args`; `spec.env`, `spec.cwd` and the three stdio specs are honoured.
    ///
    /// ## Returns
    ///
    /// The process ID of the launched process, which is suspended.
    ///
    /// ## Errors
    ///
    /// - `DebuggerError::Io` with kind `NotFound` if the program does not exist, or
    ///   `posix_spawn()` reports a missing working directory or stdio path
    /// - `DebuggerError::Io` with kind `PermissionDenied` if it may not be executed
    /// - `DebuggerError::InvalidArgument` if a string contains a NUL byte
    /// - `DebuggerError::Io` with the errno for any other spawn failure
    pub(crate) fn launch(spec: &LaunchSpec) -> Result<libc::pid_t>
    {
        use tracing::{debug, info, trace};

        info!("Launching process: {} with args: {:?}", spec.program.display(), spec.args);

        if spec.program.as_os_str().is_empty() {
            return Err(DebuggerError::InvalidArgument("Program path cannot be empty".to_string()));
        }
        // A relative path is resolved here, against the debugger's directory: with
        // `spec.cwd` set, posix_spawn would otherwise resolve it in the child after the
        // chdir action, against a different directory than the existence check.
        let program = std::path::absolute(&spec.program)?;
        if !program.exists() {
            return Err(DebuggerError::Io(Error::new(
                ErrorKind::NotFound,
                format!("Program '{}' does not exist", spec.program.display()),
            )));
        }

        // argv[0] stays as the caller wrote it; the spawn path is the absolute one.
        let program_cstr = c_string(spec.program.as_os_str().as_bytes(), "program path")?;
        let spawn_path_cstr = c_string(program.as_os_str().as_bytes(), "program path")?;

        // argv[0] is the program itself; spec.args excludes it.
        let mut arg_cstrs = vec![program_cstr.clone()];
        for arg in &spec.args {
            arg_cstrs.push(c_string(arg.as_bytes(), "argument")?);
        }
        let mut argv: Vec<*const libc::c_char> = arg_cstrs.iter().map(|s| s.as_ptr()).collect();
        argv.push(ptr::null());

        trace!("Building environment");
        let env_cstrings = Self::build_environment(&spec.env)?;
        let mut env_ptrs: Vec<*const libc::c_char> = env_cstrings.iter().map(|s| s.as_ptr()).collect();
        env_ptrs.push(ptr::null());

        unsafe {
            debug!("Initializing posix_spawn attributes");
            let mut attr = SpawnAttr(std::mem::zeroed());
            let result = ffi::posix_spawnattr_init(&mut attr.0);
            if result != 0 {
                // Not initialized: do not run the destructor.
                std::mem::forget(attr);
                return Err(spawn_error("initialize spawn attributes", result));
            }

            debug!("Setting POSIX_SPAWN_START_SUSPENDED flag");
            let flags_result = ffi::posix_spawnattr_setflags(&mut attr.0, ffi::spawn_flags::POSIX_SPAWN_START_SUSPENDED);
            if flags_result != 0 {
                return Err(spawn_error("set spawn flags", flags_result));
            }

            let mut actions = FileActions(std::mem::zeroed());
            let init_result = libc::posix_spawn_file_actions_init(&mut actions.0);
            if init_result != 0 {
                std::mem::forget(actions);
                return Err(spawn_error("initialize file actions", init_result));
            }

            Self::add_stdio(&mut actions, libc::STDIN_FILENO, &spec.stdin, "stdin")?;
            Self::add_stdio(&mut actions, libc::STDOUT_FILENO, &spec.stdout, "stdout")?;
            Self::add_stdio(&mut actions, libc::STDERR_FILENO, &spec.stderr, "stderr")?;

            if let Some(cwd) = &spec.cwd {
                let cwd_cstr = c_string(cwd.as_os_str().as_bytes(), "working directory")?;
                let result = ffi::posix_spawn_file_actions_addchdir_np(&mut actions.0, cwd_cstr.as_ptr());
                if result != 0 {
                    return Err(spawn_error("set the working directory", result));
                }
            }

            trace!("Calling posix_spawn");
            let mut pid: libc::pid_t = 0;
            let spawn_result = ffi::posix_spawn(
                &mut pid,
                spawn_path_cstr.as_ptr(),
                &actions.0,
                &attr.0, // Attributes with the START_SUSPENDED flag
                argv.as_ptr(),
                env_ptrs.as_ptr(),
            );

            if spawn_result != 0 {
                return Err(spawn_error(
                    &format!("spawn process '{}'", spec.program.display()),
                    spawn_result,
                ));
            }

            info!("Successfully spawned process with PID: {}", pid);
            Ok(pid)
        }
    }
}
