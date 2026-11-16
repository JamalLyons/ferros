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
pub mod output;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use ferros_core::{Backend, LaunchSpec, ProcessId, ProcessOwnership, Session, SessionState, StopEvent, ThreadId};
use ferros_utils::warn;

use self::commands::{CommandError, Flow};

/// The prompt printed before each command.
pub const PROMPT: &str = "(ferros) ";

/// `SIGINT`: a stop for this signal is not passed back to the program by `continue`.
const SIGINT: i32 = 2;

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
    /// Launch or attach to `target` with a new session on `backend`.
    ///
    /// Returns the debugger and the line to print (`launched process <pid>` or
    /// `attached to process <pid>`).
    ///
    /// # Errors
    ///
    /// The launch or attach error of the session.
    pub fn start(backend: Box<dyn Backend>, target: Target) -> ferros_core::Result<(Self, String)>
    {
        let mut session = Session::new(backend);
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

    /// Present SIGINT from an isolated launched program as the user's interrupt.
    /// Keep the engine's raw signal stop so continue still suppresses that signal.
    fn stop_for_display(&self, event: &StopEvent) -> StopEvent
    {
        if let Target::Launch(spec) = &self.target
            && spec.process_group == Some(ferros_core::backend::ProcessGroup::New)
            && let StopEvent::Signal { thread, signal: SIGINT } = event
        {
            return StopEvent::Interrupted { thread: Some(*thread) };
        }
        event.clone()
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
}

impl Ctx<'_>
{
    /// The session.
    pub fn session(&mut self) -> &mut Session
    {
        &mut self.debugger.session
    }

    /// Resume the stopped process (or step `step`), wait for the next stop and print it.
    ///
    /// A pending Ctrl-C from before the resume is forgotten; one pressed while the
    /// process runs interrupts it. A held `SIGINT` is not passed back to the program, any
    /// other held signal is.
    ///
    /// # Errors
    ///
    /// The session's resume, step or wait error.
    pub fn resume_and_wait(&mut self, step: Option<ThreadId>) -> Result<StopEvent, CommandError>
    {
        self.out.flush()?;
        self.interrupt.store(false, Ordering::SeqCst);
        let terminal_pid = match &self.debugger.target {
            Target::Launch(spec)
                if spec.process_group == Some(ferros_core::backend::ProcessGroup::New)
                    && spec.stdin == ferros_core::backend::StdioSpec::Inherit =>
            {
                self.debugger.session.pid()
            }
            _ => None,
        };
        let session = &mut self.debugger.session;
        let event = crate::platform::with_terminal_foreground(terminal_pid, || {
            match step {
                Some(thread) => session.step_instruction(thread)?,
                None if matches!(session.last_stop(), Some(StopEvent::Signal { signal: SIGINT, .. })) => {
                    session.resume()?;
                }
                None => session.resume_with_signal()?,
            }
            self.debugger.at_entry = false;
            interrupt::wait_for_stop(session, self.interrupt)
        })?;
        let event = self.debugger.stop_for_display(&event);
        self.print_stop(&event)?;
        Ok(event)
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
        Ok(())
    }
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
