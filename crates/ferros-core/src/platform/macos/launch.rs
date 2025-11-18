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
//! - **posix_spawn_file_actions_***(): Redirect stdout/stderr if needed
//!
//! ## References
//!
//! - [posix_spawn(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawn.3.html)

use std::ffi::CString;
use std::os::fd::RawFd;
use std::ptr;

use libc::c_int;

use crate::error::{DebuggerError, Result};
use crate::platform::macos::ffi;

/// Trait for launch operations that require access to debugger internals.
pub(crate) trait LaunchOperations
{
    /// Whether to capture process output.
    fn capture_output(&self) -> bool;

    /// Set the stdout pipe file descriptor.
    fn set_stdout_pipe(&mut self, fd: RawFd);

    /// Set the stderr pipe file descriptor.
    fn set_stderr_pipe(&mut self, fd: RawFd);
}

/// Process launch functions for macOS debugger.
pub(crate) struct LaunchManager;

impl LaunchManager
{
    /// Create a pipe pair for process output redirection.
    ///
    /// Creates a pipe and sets the `FD_CLOEXEC` flag on both file descriptors
    /// so they're closed when the process execs.
    pub(crate) fn create_pipe_pair(label: &str) -> Result<(RawFd, RawFd)>
    {
        let mut fds = [0; 2];
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                let err = std::io::Error::last_os_error();
                return Err(DebuggerError::AttachFailed(format!("Failed to create {label} pipe: {err}")));
            }

            for fd in &fds {
                if libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                    let err = std::io::Error::last_os_error();
                    // Best effort cleanup
                    let _ = libc::close(fds[0]);
                    let _ = libc::close(fds[1]);
                    return Err(DebuggerError::AttachFailed(format!(
                        "Failed to configure {label} pipe: {err}"
                    )));
                }
            }
        }

        Ok((fds[0], fds[1]))
    }

    /// Close a pipe pair.
    pub(crate) fn close_pipe_pair(pipe: &mut Option<(RawFd, RawFd)>)
    {
        if let Some((read_fd, write_fd)) = pipe.take() {
            unsafe {
                let _ = libc::close(read_fd);
                let _ = libc::close(write_fd);
            }
        }
    }

    /// Ensure a file action succeeded, cleaning up on failure.
    pub(crate) fn ensure_file_action_success(
        desc: &str,
        result: c_int,
        attr: &mut libc::posix_spawnattr_t,
        file_actions: &mut libc::posix_spawn_file_actions_t,
        stdout_pipe_fds: &mut Option<(RawFd, RawFd)>,
        stderr_pipe_fds: &mut Option<(RawFd, RawFd)>,
    ) -> Result<()>
    {
        if result == 0 {
            return Ok(());
        }

        let err = std::io::Error::from_raw_os_error(result);
        unsafe {
            let _ = libc::posix_spawn_file_actions_destroy(file_actions);
            let _ = ffi::posix_spawnattr_destroy(attr);
        }
        Self::close_pipe_pair(stdout_pipe_fds);
        Self::close_pipe_pair(stderr_pipe_fds);
        Err(DebuggerError::AttachFailed(format!("Failed to {desc}: {err}")))
    }

    /// Launch a process under debugger control.
    ///
    /// This spawns a new process using `posix_spawn()` with the
    /// `POSIX_SPAWN_START_SUSPENDED` flag, allowing breakpoints to be set
    /// before execution begins.
    ///
    /// ## Parameters
    ///
    /// - `program`: Path to the executable
    /// - `args`: Command-line arguments (first argument should be the program name)
    /// - `ops`: Launch operations trait object for accessing debugger state
    ///
    /// ## Returns
    ///
    /// The process ID of the launched process.
    ///
    /// ## Errors
    ///
    /// Returns errors if:
    /// - Program path is invalid
    /// - Arguments contain null bytes
    /// - `posix_spawn()` fails
    /// - Pipe creation fails (if output capture is enabled)
    pub(crate) fn launch<Ops: LaunchOperations>(ops: &mut Ops, program: &str, args: &[&str]) -> Result<libc::pid_t>
    {
        use tracing::{debug, info, trace};

        info!("Launching process: {} with args: {:?}", program, args);
        debug!("Validating launch parameters");

        // Validate inputs
        if program.is_empty() {
            return Err(DebuggerError::InvalidArgument("Program path cannot be empty".to_string()));
        }
        if args.is_empty() {
            return Err(DebuggerError::InvalidArgument("Arguments cannot be empty".to_string()));
        }

        // Convert program path to CString
        let program_cstr =
            CString::new(program).map_err(|e| DebuggerError::InvalidArgument(format!("Invalid program path: {}", e)))?;

        // Convert arguments to CStrings
        let mut arg_cstrs = Vec::new();
        for arg in args {
            arg_cstrs
                .push(CString::new(*arg).map_err(|e| DebuggerError::InvalidArgument(format!("Invalid argument: {}", e)))?);
        }

        // Create argv array (null-terminated)
        let mut argv: Vec<*const libc::c_char> = arg_cstrs.iter().map(|s| s.as_ptr()).collect();
        argv.push(ptr::null());

        let mut stdout_pipe_fds: Option<(RawFd, RawFd)> = None;
        let mut stderr_pipe_fds: Option<(RawFd, RawFd)> = None;

        if ops.capture_output() {
            stdout_pipe_fds = Some(Self::create_pipe_pair("stdout")?);
            stderr_pipe_fds = Some(Self::create_pipe_pair("stderr")?);
        }

        unsafe {
            debug!("Initializing posix_spawn attributes");
            // Initialize spawn attributes
            let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
            let result = ffi::posix_spawnattr_init(&mut attr);
            if result != 0 {
                Self::close_pipe_pair(&mut stdout_pipe_fds);
                Self::close_pipe_pair(&mut stderr_pipe_fds);
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to initialize spawn attributes: {}",
                    std::io::Error::from_raw_os_error(result)
                )));
            }

            debug!("Setting POSIX_SPAWN_START_SUSPENDED flag");
            // Set POSIX_SPAWN_START_SUSPENDED flag
            let flags_result = ffi::posix_spawnattr_setflags(&mut attr, ffi::spawn_flags::POSIX_SPAWN_START_SUSPENDED);
            if flags_result != 0 {
                let _ = ffi::posix_spawnattr_destroy(&mut attr);
                Self::close_pipe_pair(&mut stdout_pipe_fds);
                Self::close_pipe_pair(&mut stderr_pipe_fds);
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to set spawn flags: {}",
                    std::io::Error::from_raw_os_error(flags_result)
                )));
            }

            let mut file_actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
            let mut file_actions_initialized = false;

            if ops.capture_output() {
                file_actions_initialized = true;
                let init_result = libc::posix_spawn_file_actions_init(&mut file_actions);
                if init_result != 0 {
                    let _ = ffi::posix_spawnattr_destroy(&mut attr);
                    Self::close_pipe_pair(&mut stdout_pipe_fds);
                    Self::close_pipe_pair(&mut stderr_pipe_fds);
                    return Err(DebuggerError::AttachFailed(format!(
                        "Failed to initialize file actions: {}",
                        std::io::Error::from_raw_os_error(init_result)
                    )));
                }

                if let Some((read_fd, write_fd)) = stdout_pipe_fds {
                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, read_fd);
                    Self::ensure_file_action_success(
                        "close stdout read end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_adddup2(&mut file_actions, write_fd, libc::STDOUT_FILENO);
                    Self::ensure_file_action_success(
                        "redirect stdout",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, write_fd);
                    Self::ensure_file_action_success(
                        "close stdout write end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;
                }

                if let Some((read_fd, write_fd)) = stderr_pipe_fds {
                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, read_fd);
                    Self::ensure_file_action_success(
                        "close stderr read end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_adddup2(&mut file_actions, write_fd, libc::STDERR_FILENO);
                    Self::ensure_file_action_success(
                        "redirect stderr",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;

                    let result = libc::posix_spawn_file_actions_addclose(&mut file_actions, write_fd);
                    Self::ensure_file_action_success(
                        "close stderr write end",
                        result,
                        &mut attr,
                        &mut file_actions,
                        &mut stdout_pipe_fds,
                        &mut stderr_pipe_fds,
                    )?;
                }
            }

            trace!("Calling posix_spawn");
            // Spawn the process
            let mut pid: libc::pid_t = 0;
            let spawn_result = ffi::posix_spawn(
                &mut pid,
                program_cstr.as_ptr(),
                if file_actions_initialized {
                    &file_actions
                } else {
                    ptr::null()
                },
                &attr, // Use attributes with START_SUSPENDED flag
                argv.as_ptr(),
                ptr::null(), // Use current environment
            );

            if file_actions_initialized {
                let _ = libc::posix_spawn_file_actions_destroy(&mut file_actions);
            }

            // Clean up attributes
            let _ = ffi::posix_spawnattr_destroy(&mut attr);

            if spawn_result != 0 {
                Self::close_pipe_pair(&mut stdout_pipe_fds);
                Self::close_pipe_pair(&mut stderr_pipe_fds);
                return Err(DebuggerError::AttachFailed(format!(
                    "Failed to spawn process '{}': {}",
                    program,
                    std::io::Error::from_raw_os_error(spawn_result)
                )));
            }

            if let Some((read_fd, write_fd)) = stdout_pipe_fds.take() {
                let _ = libc::close(write_fd);
                ops.set_stdout_pipe(read_fd);
            }

            if let Some((read_fd, write_fd)) = stderr_pipe_fds.take() {
                let _ = libc::close(write_fd);
                ops.set_stderr_pipe(read_fd);
            }

            info!("Successfully spawned process with PID: {}", pid);
            Ok(pid)
        }
    }
}
