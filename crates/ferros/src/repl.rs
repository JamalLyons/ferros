use std::path::PathBuf;

use ferros_core::prelude::*;
use libc::SIGTERM;
use log::LevelFilter;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

/// REPL (Read-Eval-Print Loop) for interactive debugging
pub struct Repl
{
    debugger: Box<dyn FerrosDebugger>,
    pid: u32,
    kill_on_exit: bool,
}

impl Repl
{
    /// Create a new REPL with an attached debugger
    ///
    /// `kill_on_exit` indicates whether we should terminate the target when the REPL exits.
    /// This is true for launched processes (we own them) and false for attached processes.
    pub fn new(debugger: Box<dyn FerrosDebugger>, pid: u32, kill_on_exit: bool) -> FerrosResult<Self>
    {
        Ok(Self {
            debugger,
            pid,
            kill_on_exit,
        })
    }

    /// Run the REPL loop
    pub fn run(&mut self) -> FerrosResult<()>
    {
        // Disable rustyline debug logs by setting log crate's max level to INFO
        // Rustyline uses the log crate internally, so we filter it at the log level
        // This prevents DEBUG and TRACE logs from rustyline from appearing
        let _ = log::set_max_level(LevelFilter::Info);

        let mut rl =
            DefaultEditor::new().map_err(|e| FerrosError::InvalidArgument(format!("Failed to initialize REPL: {}", e)))?;

        // Set history file path to the user's home directory
        let history_path = if is_running_as_root() {
            // When running as root (via sudo), use the real user's home directory
            if let Ok(sudo_user) = std::env::var("SUDO_USER") {
                // Construct home directory path (works on macOS and Linux)
                let home = if cfg!(target_os = "macos") {
                    format!("/Users/{}", sudo_user)
                } else {
                    format!("/home/{}", sudo_user)
                };
                PathBuf::from(home).join(".ferros_history")
            } else {
                // Fallback: use current directory if SUDO_USER not available
                PathBuf::from(".ferros_history")
            }
        } else {
            // Normal case: use current user's home directory
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".ferros_history")
        };
        let _ = rl.load_history(&history_path);

        println!("Ferros Debugger REPL");
        println!("Type 'help' for available commands, 'quit' to exit.");

        loop {
            let readline = rl.readline("(ferros) ");
            match readline {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    // Add to history
                    let _ = rl.add_history_entry(line);

                    // Parse and execute command
                    match self.execute_command(line) {
                        Ok(should_continue) => {
                            if !should_continue {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("Error: {}", e);
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    println!("^C");
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    println!("\nExiting...");
                    break;
                }
                Err(err) => {
                    eprintln!("Error: {:?}", err);
                    break;
                }
            }
        }

        // Save history
        if let Err(e) = rl.save_history(&history_path) {
            eprintln!("Warning: Failed to save history: {}", e);
        } else {
            if is_running_as_root() {
                fix_file_permissions(&history_path);
            }
        }

        // If we launched the process, ensure it is terminated when the REPL exits.
        if self.kill_on_exit {
            unsafe {
                let _ = libc::kill(self.pid as libc::pid_t, SIGTERM);
            }
        }

        Ok(())
    }

    /// Execute a command and return whether to continue the REPL loop
    fn execute_command(&mut self, input: &str) -> FerrosResult<bool>
    {
        crate::commands::execute_repl_command(&mut self.debugger, self.pid, input)
    }
}

/// Check if the current process is running as root (UID 0)
fn is_running_as_root() -> bool
{
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false // TODO: Implement this for Windows
    }
}

/// A helper function to fix file permissions to be readable/writable by the real user when running as root.
/// Without this function, the user would have to manually change the permissions of the history file themselves.
fn fix_file_permissions(path: &PathBuf)
{
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        // Get the real user's UID from SUDO_UID environment variable
        if let Ok(sudo_uid_str) = std::env::var("SUDO_UID") {
            if let Ok(uid) = sudo_uid_str.parse::<u32>() {
                // Get the real user's GID from SUDO_GID
                let gid = std::env::var("SUDO_GID")
                    .ok()
                    .and_then(|g| g.parse::<u32>().ok())
                    .unwrap_or(uid); // Fallback to UID if GID not available

                // Convert path to C string
                let path_cstr = std::ffi::CString::new(path.as_os_str().as_bytes()).ok();
                if let Some(path_cstr) = path_cstr {
                    // Change ownership to the real user
                    unsafe {
                        let _ = libc::chown(path_cstr.as_ptr(), uid as libc::uid_t, gid as libc::gid_t);
                    }
                }

                // Set permissions to 644 (rw-r--r--)
                if let Ok(mut perms) = std::fs::metadata(path).map(|m| m.permissions()) {
                    perms.set_mode(0o644);
                    let _ = std::fs::set_permissions(path, perms);
                }
            }
        }
    }
}
