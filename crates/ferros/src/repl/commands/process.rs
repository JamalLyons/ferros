//! Process control: `run`, `continue`, `interrupt`, `kill`, `status`, `quit`.

use ferros_core::SessionState;

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args};
use crate::repl::output::describe_stop;
use crate::repl::{Ctx, Target};

/// `run`: start the program, or restart it after it exited.
pub struct Run;

impl ReplCommand for Run
{
    fn names(&self) -> &'static [&'static str]
    {
        &["run", "r"]
    }

    fn summary(&self) -> &'static str
    {
        "start the program from its entry stop; after it exited, relaunch it stopped at entry"
    }

    fn help(&self) -> &'static str
    {
        "run (alias: r)\n\nResume a just-launched program from its entry stop and wait for the next stop \
         (as 'continue' does). After the program exited or was killed, launch it again with the same \
         arguments and stop at its entry, with every file:line and function breakpoint resolved again \
         (address breakpoints are deleted, since addresses change between runs); then 'run' (or \
         'continue') starts it. An attached process cannot be restarted."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("run", args)?;
        match ctx.session().state() {
            SessionState::Stopped if ctx.debugger.at_entry => {}
            SessionState::Stopped | SessionState::Running => {
                return Err(CommandError::new("the program has already started (use 'continue')"));
            }
            SessionState::NotStarted | SessionState::Exited => {
                let Target::Launch(spec) = ctx.debugger.target.clone() else {
                    return Err(CommandError::new("an attached process cannot be restarted"));
                };
                let pid = ctx.session().launch(&spec)?;
                ctx.debugger.at_entry = true;
                writeln!(ctx.out, "launched process {} (stopped at entry; address breakpoints were cleared)", pid.0)?;
                // A breakpoint that did not resolve in the new process would never stop it.
                let pending: Vec<_> = ctx
                    .session()
                    .logical_breakpoints()
                    .iter()
                    .filter_map(|breakpoint| Some((breakpoint.id.raw(), breakpoint.location.to_string(), breakpoint.pending.clone()?)))
                    .collect();
                for (id, location, reason) in pending {
                    writeln!(ctx.out, "breakpoint {id} at {location} is pending: {reason}")?;
                }
                return Ok(Flow::Continue);
            }
        }
        ctx.resume_and_wait(None)?;
        Ok(Flow::Continue)
    }
}

/// `continue`: resume and wait for the next stop.
pub struct Continue;

impl ReplCommand for Continue
{
    fn names(&self) -> &'static [&'static str]
    {
        &["continue", "c"]
    }

    fn summary(&self) -> &'static str
    {
        "resume the program and wait for it to stop (Ctrl-C interrupts it)"
    }

    fn help(&self) -> &'static str
    {
        "continue (alias: c)\n\nResume every thread and wait for the next stop, which is printed as one of:\n  \
         stopped: breakpoint <id> at <address> (thread <tid>), then '<function> at <file>:<line>' and the \
         source line when known\n  stopped: signal <number> (thread <tid>)\n  \
         stopped: interrupted\n  process exited with status <code>\n  process killed by signal <number>\nA \
         signal the program stopped for is passed on to it, except SIGINT and SIGTSTP. Ctrl-C while the program \
         runs interrupts it and returns to the prompt; Ctrl-Z interrupts it, then suspends ferros."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("continue", args)?;
        ctx.resume_and_wait(None)?;
        Ok(Flow::Continue)
    }
}

/// `interrupt`: stop a running program.
pub struct Interrupt;

impl ReplCommand for Interrupt
{
    fn names(&self) -> &'static [&'static str]
    {
        &["interrupt"]
    }

    fn summary(&self) -> &'static str
    {
        "stop the program (Ctrl-C does this while it runs)"
    }

    fn help(&self) -> &'static str
    {
        "interrupt\n\nAsk the program to stop. Commands only run while the program is stopped, so this does \
         nothing at the prompt; press Ctrl-C to interrupt a program running under 'continue'."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("interrupt", args)?;
        ctx.session().interrupt()?;
        writeln!(ctx.out, "the process is already stopped")?;
        Ok(Flow::Continue)
    }
}

/// `kill`: terminate the program.
pub struct Kill;

impl ReplCommand for Kill
{
    fn names(&self) -> &'static [&'static str]
    {
        &["kill"]
    }

    fn summary(&self) -> &'static str
    {
        "terminate the program ('run' launches it again)"
    }

    fn help(&self) -> &'static str
    {
        "kill\n\nTerminate the program with SIGKILL. Its breakpoints are forgotten. 'run' launches a killed \
         program again."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("kill", args)?;
        let event = ctx.session().kill()?;
        ctx.print_stop(&event)?;
        Ok(Flow::Continue)
    }
}

/// `status`: what the session is doing.
pub struct Status;

impl ReplCommand for Status
{
    fn names(&self) -> &'static [&'static str]
    {
        &["status"]
    }

    fn summary(&self) -> &'static str
    {
        "show whether the program is stopped or exited, and why"
    }

    fn help(&self) -> &'static str
    {
        "status\n\nPrint the session state: 'status: stopped (pid <pid>, thread <tid>)' followed by the last \
         stop, 'status: exited' followed by how it ended, or 'status: no process'."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("status", args)?;
        let session = ctx.session();
        let last = session.last_stop().cloned();
        let (head, detail) = match session.state() {
            SessionState::Stopped => {
                let pid = session.pid().map_or(0, |pid| pid.0);
                let head = match session.selected_thread() {
                    Some(thread) => format!("status: stopped (pid {pid}, thread {})", thread.raw()),
                    None => format!("status: stopped (pid {pid})"),
                };
                let detail = match &last {
                    Some(event) => describe_stop(session, event),
                    None if ctx.debugger.at_entry => "at entry".to_owned(),
                    None => "no stop event".to_owned(),
                };
                (head, Some(detail))
            }
            SessionState::Running => ("status: running".to_owned(), None),
            SessionState::Exited => ("status: exited".to_owned(), last.map(|event| describe_stop(session, &event))),
            SessionState::NotStarted => ("status: no process".to_owned(), None),
        };
        writeln!(ctx.out, "{head}")?;
        if let Some(detail) = detail {
            writeln!(ctx.out, "  {detail}")?;
        }
        Ok(Flow::Continue)
    }
}

/// `quit`: end the session.
pub struct Quit;

impl ReplCommand for Quit
{
    fn names(&self) -> &'static [&'static str]
    {
        &["quit", "q", "exit"]
    }

    fn summary(&self) -> &'static str
    {
        "end the session: kill a launched program, detach from an attached one"
    }

    fn help(&self) -> &'static str
    {
        "quit (aliases: q, exit)\n\nEnd the session. A launched program is killed; an attached process is \
         detached and keeps running. The end of input does the same."
    }

    fn run(&self, _ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("quit", args)?;
        Ok(Flow::Quit)
    }
}
