mod platform;
mod repl;

use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use ferros_core::backend::{ProcessGroup, StdioSpec};
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
        /// Feed FILE to the program's stdin. Without it the program shares the terminal or
        /// the batch-mode stdin, and reads /dev/null when commands come from a pipe
        #[arg(long, value_name = "FILE")]
        stdin: Option<PathBuf>,
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

    let (script, mut target, stdin_file) = match cli.command {
        Commands::FindLogs => {
            println!("{}", log_directory().display());
            return ExitCode::SUCCESS;
        }
        Commands::Launch {
            script,
            stdin,
            program,
            args,
        } => {
            let spec = LaunchSpec::new(absolute(&program))
                .args(args)
                .process_group(ProcessGroup::New);
            (script, Target::Launch(spec), stdin)
        }
        Commands::Attach { script, pid } => (script, Target::Attach(ProcessId::from(pid)), None),
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

    // Decided before the launch: the program's stdin depends on where commands come from.
    let mode = Mode::of(lines, io::stdin().is_terminal());
    if let Target::Launch(spec) = &mut target {
        let stdin_file = stdin_file.map(|path| absolute(&path));
        if let Some(path) = &stdin_file
            && let Err(error) = std::fs::File::open(path)
        {
            eprintln!("error: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
        spec.stdin = debuggee_stdin(stdin_file, &mode);
    }

    if run_session(target, mode) {
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

/// Where the commands come from.
#[derive(Debug, PartialEq, Eq)]
enum Mode
{
    /// `--batch` and `--command` lines; stdin is not read.
    Batch(Vec<String>),
    /// A line editor on the terminal.
    Terminal,
    /// Lines read from stdin, which is not a terminal (a pipe or a file).
    Pipe,
}

impl Mode
{
    /// The mode for the batch `lines` (if any) and whether stdin is a terminal.
    fn of(lines: Option<Vec<String>>, stdin_is_terminal: bool) -> Self
    {
        match lines {
            Some(lines) => Self::Batch(lines),
            None if stdin_is_terminal => Self::Terminal,
            None => Self::Pipe,
        }
    }
}

/// The launched program's stdin: `file` when given; otherwise `/dev/null` in pipe mode,
/// where ferros reads its commands from stdin and the program must not take them, and
/// ferros's own stdin (the terminal, or whatever batch mode leaves unread) in the others.
fn debuggee_stdin(file: Option<PathBuf>, mode: &Mode) -> StdioSpec
{
    match (file, mode) {
        (Some(path), _) => StdioSpec::Path(path),
        (None, Mode::Pipe) => StdioSpec::Null,
        (None, Mode::Batch(_) | Mode::Terminal) => StdioSpec::Inherit,
    }
}

/// Start the session and run the command loop. Returns `true` when nothing failed.
fn run_session(target: Target, mode: Mode) -> bool
{
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    let stdin = io::stdin();
    let input = match mode {
        Mode::Batch(lines) => Input::Lines(lines.into()),
        Mode::Terminal => match repl::editor() {
            Ok(editor) => editor,
            Err(error) => {
                let _ = writeln!(err, "error: cannot start the line editor: {error}");
                Input::Reader(stdin.lock())
            }
        },
        Mode::Pipe => Input::Reader(stdin.lock()),
    };
    // rustyline installs its own SIGINT handler when the editor is created. Install
    // ours afterwards so signals received while a command runs reach wait_for_stop.
    // At the prompt Ctrl-C is a raw-mode key, handled by the editor itself.
    let interrupt = repl::interrupt::install();
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

    if matches!(&input, Input::Editor(..)) {
        let _ = writeln!(out, "Type 'help' for the commands; 'quit' or Ctrl-D ends the session.");
    }
    repl::run(&mut debugger, input, &mut out, &mut err, &interrupt)
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn mode_follows_the_script_then_the_terminal()
    {
        assert_eq!(
            Mode::of(Some(vec!["status".into()]), false),
            Mode::Batch(vec!["status".into()])
        );
        assert_eq!(Mode::of(Some(Vec::new()), true), Mode::Batch(Vec::new()));
        assert_eq!(Mode::of(None, true), Mode::Terminal);
        assert_eq!(Mode::of(None, false), Mode::Pipe);
    }

    #[test]
    fn only_pipe_mode_closes_the_program_stdin()
    {
        assert_eq!(debuggee_stdin(None, &Mode::Pipe), StdioSpec::Null);
        assert_eq!(debuggee_stdin(None, &Mode::Terminal), StdioSpec::Inherit);
        assert_eq!(debuggee_stdin(None, &Mode::Batch(Vec::new())), StdioSpec::Inherit);
    }

    #[test]
    fn an_explicit_stdin_file_wins_in_every_mode()
    {
        let file = PathBuf::from("/tmp/input.txt");
        let expected = StdioSpec::Path(file.clone());
        assert_eq!(debuggee_stdin(Some(file.clone()), &Mode::Pipe), expected);
        assert_eq!(debuggee_stdin(Some(file.clone()), &Mode::Terminal), expected);
        assert_eq!(debuggee_stdin(Some(file), &Mode::Batch(Vec::new())), expected);
    }
}
