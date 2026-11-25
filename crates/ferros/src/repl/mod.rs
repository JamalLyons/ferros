//! The command loop: one [`Session`], driven by lines of commands.
//!
//! Commands come from one of three [`Input`] sources:
//!
//! - [`Input::Editor`]: an interactive terminal (rustyline). The history file is
//!   `$FERROS_HISTORY` when that is set and not empty, otherwise `~/.ferros_history`
//!   (see [`history_path`]).
//! - [`Input::Reader`]: plain lines from a reader (stdin that is not a terminal). The
//!   prompt `(ferros) ` is printed before each line is read and the line is echoed after
//!   it, so a driver can wait for the prompt before sending the next command.
//! - [`Input::Lines`]: a fixed list (`--batch` and `--command`). Each command is printed
//!   as `(ferros) <command>` before its output; stdin is not read.
//!
//! Empty lines and lines starting with `#` are skipped. A failed command prints
//! `error: <message>` on the error stream and the loop continues; [`run`] reports
//! whether any command failed. At `quit` or the end of input a launched process is
//! killed and an attached one is detached.

pub mod commands;
pub mod interrupt;
pub mod jobs;
pub mod output;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use ferros_core::backend::ProcessGroup;
use ferros_core::{LaunchSpec, ProcessId, ProcessOwnership, Session, SessionState, StopEvent, ThreadId};
use ferros_utils::warn;

use self::commands::{CommandError, Flow};
use self::jobs::JobControl;

/// The prompt printed before each command.
pub const PROMPT: &str = "(ferros) ";

/// `SIGINT` and `SIGTSTP`: a stop for one of these signals is not passed back to the
/// program by `continue`. With `launch --terminal`, Ctrl-C and Ctrl-Z reach the program
/// as these signals; passing `SIGTSTP` on would stop it for the shell, outside ferros.
const SIGINT: i32 = 2;
const SIGTSTP: i32 = 18;
/// `SIGTTIN` and `SIGTTOU`: a launched program, in a background process group, used the
/// terminal. `continue` passes the signal back and the program stops again.
const SIGTTIN: i32 = 21;
const SIGTTOU: i32 = 22;

/// How the session got its process, and how `run` starts it again.
#[derive(Debug, Clone)]
pub enum Target
{
    /// Started from this spec; `run` relaunches it after it exited.
    Launch(LaunchSpec),
    /// Attached to this pid; it cannot be restarted.
    Attach(ProcessId),
}

/// The session and what the commands remember about it.
#[derive(Debug)]
pub struct Debugger
{
    /// The engine.
    pub session: Session,
    /// How the process was obtained.
    pub target: Target,
    /// The process is at its initial stop and has never run.
    pub at_entry: bool,
}

impl Debugger
{
    /// Launch or attach to `target` with `session`, which has no process yet.
    ///
    /// Returns the debugger and the line to print (`launched process <pid>` or
    /// `attached to process <pid>`).
    ///
    /// # Errors
    ///
    /// The launch or attach error of the session.
    pub fn start(mut session: Session, target: Target) -> ferros_core::Result<(Self, String)>
    {
        let message = match &target {
            Target::Launch(spec) => format!("launched process {}", session.launch(spec)?.0),
            Target::Attach(pid) => {
                session.attach(*pid)?;
                format!("attached to process {}", pid.0)
            }
        };
        Ok((
            Self {
                session,
                target,
                at_entry: true,
            },
            message,
        ))
    }

    /// Release the process: kill it if it was launched, detach if attached.
    ///
    /// Returns the line to print, if there was a process.
    ///
    /// # Errors
    ///
    /// The kill or detach error of the session.
    pub fn finish(&mut self) -> ferros_core::Result<Option<String>>
    {
        let Some(pid) = self.session.pid() else {
            return Ok(None);
        };
        match (self.session.ownership(), self.session.state()) {
            (Some(ProcessOwnership::Launched), _) => {
                self.session.kill()?;
                Ok(Some(format!("killed process {}", pid.0)))
            }
            (Some(ProcessOwnership::Attached), SessionState::Stopped) => {
                self.session.detach()?;
                Ok(Some(format!("detached from process {}", pid.0)))
            }
            // A running attached process is interrupted and released when the session
            // is dropped; the loop never leaves one running.
            _ => Ok(None),
        }
    }
}

/// What a command can reach.
pub struct Ctx<'a>
{
    /// The session and its bookkeeping.
    pub debugger: &'a mut Debugger,
    /// Standard output of the command.
    pub out: &'a mut dyn Write,
    /// Set by the Ctrl-C handler.
    pub interrupt: &'a AtomicBool,
    /// Ctrl-Z and the terminal foreground while the program runs.
    pub jobs: &'a mut dyn JobControl,
}

impl Ctx<'_>
{
    pub fn session(&mut self) -> &mut Session
    {
        &mut self.debugger.session
    }

    /// Resume the stopped process (or step `step`), wait for the next stop and print it.
    ///
    /// A held `SIGINT` or `SIGTSTP` is not passed back to the program, any other held
    /// signal is. A fault holds no signal, so continuing after one delivers nothing
    /// for it (signals still held for other threads are passed on) and the program
    /// retries the faulting instruction. See [`run_program`](Self::run_program) for Ctrl-C and Ctrl-Z.
    ///
    /// # Errors
    ///
    /// The session's resume, step or wait error.
    pub fn resume_and_wait(&mut self, step: Option<ThreadId>) -> Result<StopEvent, CommandError>
    {
        self.run_program(|session| match step {
            Some(thread) => session.step_instruction(thread),
            None if matches!(
                session.last_stop(),
                Some(StopEvent::Signal {
                    signal: SIGINT | SIGTSTP,
                    ..
                })
            ) =>
            {
                session.resume()
            }
            None => session.resume_with_signal(),
        })
    }

    /// Let the program run with `start` (a resume or a step), wait for the next stop and
    /// print it.
    ///
    /// A pending Ctrl-C from before the start is forgotten; one pressed while the process
    /// runs interrupts it. Ctrl-Z while it runs interrupts it too, and once the stop is
    /// printed ferros stops itself; after `fg` the program is still stopped. With
    /// `launch --terminal` the program holds the terminal foreground until it stops.
    ///
    /// # Errors
    ///
    /// The error of `start` or of the wait, or a job control error.
    pub fn run_program(
        &mut self,
        start: impl FnOnce(&mut Session) -> ferros_core::Result<()>,
    ) -> Result<StopEvent, CommandError>
    {
        self.out.flush()?;
        self.interrupt.store(false, Ordering::SeqCst);
        let group = self.program_group();
        self.jobs.program_runs(group).map_err(|error| job_error(&error))?;
        if let Err(error) = start(&mut self.debugger.session) {
            let _ = self.jobs.program_stopped();
            return Err(error.into());
        }
        self.debugger.at_entry = false;
        let waited = interrupt::wait_for_stop(&mut self.debugger.session, self.interrupt, &*self.jobs);
        let suspend = self.jobs.program_stopped();
        let event = waited?;
        self.print_stop(&event)?;
        if suspend.map_err(|error| job_error(&error))? {
            writeln!(self.out, "ferros suspended; after 'fg' the program is still stopped")?;
            self.out.flush()?;
            self.jobs.suspend().map_err(|error| job_error(&error))?;
        }
        Ok(event)
    }

    /// The program's own process group: a launched program in a new group leads it.
    fn program_group(&self) -> Option<ProcessId>
    {
        let session = &self.debugger.session;
        match (&self.debugger.target, session.ownership()) {
            (Target::Launch(spec), Some(ProcessOwnership::Launched)) if spec.process_group == Some(ProcessGroup::New) => {
                session.pid()
            }
            _ => None,
        }
    }

    /// Print one line describing `event`.
    ///
    /// # Errors
    ///
    /// An output error.
    pub fn print_stop(&mut self, event: &StopEvent) -> Result<(), CommandError>
    {
        let line = output::describe_stop(&mut self.debugger.session, event);
        writeln!(self.out, "{line}")?;
        if let StopEvent::Signal {
            signal: SIGTTIN | SIGTTOU,
            ..
        } = event
        {
            writeln!(
                self.out,
                "  the program used the terminal from its background process group; relaunch with `--stdin FILE`, or with \
                 `--terminal` to type its input"
            )?;
        }
        Ok(())
    }
}

/// A job control failure as a command error.
fn job_error(error: &io::Error) -> CommandError
{
    CommandError::new(format!("job control: {error}"))
}

/// Where commands come from. See the [module documentation](self).
pub enum Input<R>
{
    /// A fixed list of commands (batch mode).
    Lines(VecDeque<String>),
    /// Lines read from a reader, with prompt and echo.
    Reader(R),
    /// An interactive line editor and its history file.
    Editor(Box<rustyline::DefaultEditor>, Option<PathBuf>),
}

impl<R: BufRead> Input<R>
{
    /// The next command line, or `None` at the end of input.
    fn next_line(&mut self, out: &mut dyn Write) -> io::Result<Option<String>>
    {
        match self {
            Self::Lines(lines) => {
                // Skipped lines are not echoed: the transcript shows only what ran.
                while let Some(line) = lines.pop_front() {
                    if !is_skipped(&line) {
                        writeln!(out, "{PROMPT}{line}")?;
                        return Ok(Some(line));
                    }
                }
                Ok(None)
            }
            Self::Reader(reader) => {
                write!(out, "{PROMPT}")?;
                out.flush()?;
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 {
                    writeln!(out)?;
                    return Ok(None);
                }
                let line = line.trim_end_matches(['\n', '\r']).to_owned();
                writeln!(out, "{line}")?;
                Ok(Some(line))
            }
            Self::Editor(editor, _) => loop {
                match editor.readline(PROMPT) {
                    Ok(line) => {
                        if !line.trim().is_empty() {
                            let _ = editor.add_history_entry(line.as_str());
                        }
                        return Ok(Some(line));
                    }
                    // Ctrl-C at the prompt discards the line.
                    Err(rustyline::error::ReadlineError::Interrupted) => {}
                    Err(rustyline::error::ReadlineError::Eof) => return Ok(None),
                    Err(error) => return Err(io::Error::other(error)),
                }
            },
        }
    }

    /// Save the editor history, if any.
    fn finish(&mut self)
    {
        if let Self::Editor(editor, Some(path)) = self
            && let Err(error) = editor.save_history(path)
        {
            warn!("could not save the history to {}: {error}", path.display());
        }
    }
}

/// An interactive editor with the history loaded from [`history_path`].
///
/// # Errors
///
/// The editor's initialisation error.
pub fn editor<R>() -> rustyline::Result<Input<R>>
{
    let mut editor = rustyline::DefaultEditor::new()?;
    let history = history_path(std::env::var_os("FERROS_HISTORY"), std::env::var_os("HOME"));
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }
    Ok(Input::Editor(Box::new(editor), history))
}

/// The history file of the interactive editor, from the values of `FERROS_HISTORY` and
/// `HOME`: `$FERROS_HISTORY` when it is set and not empty, otherwise
/// `$HOME/.ferros_history`, or no history at all when neither is set.
#[must_use]
pub fn history_path(ferros_history: Option<OsString>, home: Option<OsString>) -> Option<PathBuf>
{
    match ferros_history {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => home.map(|home| PathBuf::from(home).join(".ferros_history")),
    }
}

/// Whether `line` is blank or a `#` comment.
fn is_skipped(line: &str) -> bool
{
    let line = line.trim();
    line.is_empty() || line.starts_with('#')
}

/// Run one command line. Returns `None` for a line that is skipped.
fn run_line(ctx: &mut Ctx<'_>, line: &str) -> Option<Result<Flow, CommandError>>
{
    if is_skipped(line) {
        return None;
    }
    let line = line.trim();
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    Some(match commands::find(name) {
        Some(command) => command.run(ctx, args.trim()),
        None => Err(CommandError::new(format!("unknown command '{name}' (try 'help')"))),
    })
}

/// Read and run commands until `quit` or the end of input, then release the process.
///
/// Returns `true` when every command succeeded (and the process was released cleanly).
pub fn run<R: BufRead>(
    debugger: &mut Debugger,
    mut input: Input<R>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interrupt: &AtomicBool,
    jobs: &mut dyn JobControl,
) -> bool
{
    let mut ok = true;
    loop {
        let line = match input.next_line(out) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                let _ = writeln!(err, "error: reading commands: {error}");
                ok = false;
                break;
            }
        };
        let mut ctx = Ctx {
            debugger,
            out,
            interrupt,
            jobs: &mut *jobs,
        };
        let result = run_line(&mut ctx, &line);
        let _ = out.flush();
        match result {
            None | Some(Ok(Flow::Continue)) => {}
            Some(Ok(Flow::Quit)) => break,
            Some(Err(error)) => {
                ok = false;
                let _ = writeln!(err, "error: {error}");
                let _ = err.flush();
            }
        }
    }
    input.finish();
    match debugger.finish() {
        Ok(Some(line)) => {
            let _ = writeln!(out, "{line}");
        }
        Ok(None) => {}
        Err(error) => {
            ok = false;
            let _ = writeln!(err, "error: releasing the process: {error}");
        }
    }
    let _ = out.flush();
    let _ = err.flush();
    ok
}

#[cfg(test)]
mod tests;
