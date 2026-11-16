mod platform;
mod repl;

use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use ferros_core::{LaunchSpec, ProcessId};
use ferros_utils::{LogFormat, LogLevel, info, init_logging_to_file};

use crate::platform::create_backend;
use crate::repl::{Debugger, Input, Target};

/// A Rust-native debugger with hybrid MIR and system-level introspection.
#[derive(Parser, Debug)]
#[command(name = "ferros")]
#[command(version)]
#[command(about = "A Rust-native debugger with hybrid MIR and system-level introspection", long_about = None)]
struct Cli
{
    /// Set the log level (error, warn, info, debug, trace); overrides `RUST_LOG`.
    /// Logs go to a file under ~/.ferros (see `ferros find-logs`), never to the terminal.
    #[arg(long, value_name = "LEVEL", global = true)]
    log_level: Option<String>,

    /// Set the log format of the log file (pretty, json); overrides `FERROS_LOG_FORMAT`.
    #[arg(long, value_name = "FORMAT", global = true)]
    log_format: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands
{
    /// Launch a program, stopped at its entry, and debug it
    ///
    /// Program arguments go after `--`: `ferros launch ./prog -- --flag value`.
    Launch
    {
        #[command(flatten)]
        script: Script,
        /// Path to the executable to launch
        program: PathBuf,
        /// Arguments passed to the program (after `--`)
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
    /// Attach to a running process by PID and debug it
    Attach
    {
        #[command(flatten)]
        script: Script,
        /// Process ID (PID) to attach to
        pid: u32,
    },
    /// Print the directory that holds the Ferros log files
    FindLogs,
}

/// Commands to run instead of reading them interactively.
#[derive(Args, Debug)]
struct Script
{
    /// Run the commands in FILE (one per line; `#` starts a comment), then end the session
    #[arg(long, value_name = "FILE")]
    batch: Option<PathBuf>,
    /// Run CMD (repeatable; runs after --batch), then end the session
    #[arg(long = "command", short = 'c', value_name = "CMD")]
    commands: Vec<String>,
}

impl Script
{
    /// The batch command list, or `None` when commands come from stdin.
    fn lines(&self) -> io::Result<Option<Vec<String>>>
    {
        if self.batch.is_none() && self.commands.is_empty() {
            return Ok(None);
        }
        let mut lines = Vec::new();
        if let Some(path) = &self.batch {
            let text = std::fs::read_to_string(path)
                .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
            lines.extend(text.lines().map(str::to_owned));
        }
        lines.extend(self.commands.iter().cloned());
        Ok(Some(lines))
    }
}

fn log_directory() -> PathBuf
{
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), |home| PathBuf::from(home).join(".ferros"))
}

fn main() -> ExitCode
{
    let cli = Cli::parse();

    let level = match cli.log_level.as_deref().map(str::parse::<LogLevel>).transpose() {
        Ok(level) => level,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let format = cli
        .log_format
        .clone()
        .or_else(|| std::env::var("FERROS_LOG_FORMAT").ok())
        .map_or(Ok(LogFormat::Pretty), |text| text.parse::<LogFormat>());
    let format = match format {
        Ok(format) => format,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let (script, target) = match cli.command {
        Commands::FindLogs => {
            println!("{}", log_directory().display());
            return ExitCode::SUCCESS;
        }
        Commands::Launch { script, program, args } => {
            let program = absolute(&program);
            (script, Target::Launch(LaunchSpec::new(program).args(args)))
        }
        Commands::Attach { script, pid } => (script, Target::Attach(ProcessId::from(pid))),
    };

    // Logs go to a file so the session output stays deterministic (no timestamps, no colour).
    if let Err(error) = init_logging_to_file(level, format) {
        eprintln!("warning: logging is disabled: {error}");
    }

    let lines = match script.lines() {
        Ok(lines) => lines,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    if run_session(target, lines) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `program` as an absolute path (the backend spawns it by path).
fn absolute(program: &Path) -> PathBuf
{
    if program.is_absolute() {
        program.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| program.to_path_buf(), |cwd| cwd.join(program))
    }
}

/// Start the session and run the command loop. Returns `true` when nothing failed.
fn run_session(target: Target, lines: Option<Vec<String>>) -> bool
{
    let interrupt = repl::interrupt::install();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    let started = create_backend().and_then(|backend| Debugger::start(backend, target));
    let (mut debugger, message) = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return false;
        }
    };
    info!("{message}");
    let _ = writeln!(out, "{message}");

    let stdin = io::stdin();
    let input = match lines {
        Some(lines) => Input::Lines(lines.into()),
        None if stdin.is_terminal() => match repl::editor() {
            Ok(editor) => {
                let _ = writeln!(out, "Type 'help' for the commands; 'quit' or Ctrl-D ends the session.");
                editor
            }
            Err(error) => {
                let _ = writeln!(err, "error: cannot start the line editor: {error}");
                Input::Reader(stdin.lock())
            }
        },
        None => Input::Reader(stdin.lock()),
    };
    repl::run(&mut debugger, input, &mut out, &mut err, &interrupt)
}
