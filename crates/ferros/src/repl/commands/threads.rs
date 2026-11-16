//! Threads: `threads`, `thread <n>`.

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args, parse_number};
use crate::repl::Ctx;

/// `threads`: list the threads.
pub struct Threads;

impl ReplCommand for Threads
{
    fn names(&self) -> &'static [&'static str]
    {
        &["threads"]
    }

    fn summary(&self) -> &'static str
    {
        "list the threads; '*' marks the selected one"
    }

    fn help(&self) -> &'static str
    {
        "threads\n\nList the threads as '<marker> <index> thread <tid>', where the marker is '*' for the \
         selected thread. 'thread <index>' selects one."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("threads", args)?;
        let threads = ctx.session().threads()?;
        let selected = ctx.session().selected_thread();
        for (index, thread) in threads.iter().enumerate() {
            let marker = if Some(*thread) == selected { '*' } else { ' ' };
            writeln!(ctx.out, "{marker} {index} thread {}", thread.raw())?;
        }
        Ok(Flow::Continue)
    }
}

/// `thread <n>`: select a thread by its index in `threads`.
pub struct Thread;

impl ReplCommand for Thread
{
    fn names(&self) -> &'static [&'static str]
    {
        &["thread"]
    }

    fn summary(&self) -> &'static str
    {
        "select a thread: thread <index> (as listed by 'threads')"
    }

    fn help(&self) -> &'static str
    {
        "thread [index]\n\nSelect the thread at this index of the 'threads' list; 'registers' and 'stepi' use \
         the selected thread. Without an index, print the selected thread."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let threads = ctx.session().threads()?;
        if args.is_empty() {
            let selected = ctx.session().selected_thread();
            match selected.and_then(|thread| threads.iter().position(|t| *t == thread).map(|i| (i, thread))) {
                Some((index, thread)) => writeln!(ctx.out, "thread {index} (thread {})", thread.raw())?,
                None => writeln!(ctx.out, "no thread is selected")?,
            }
            return Ok(Flow::Continue);
        }
        let index = parse_number(args)?;
        let thread = usize::try_from(index)
            .ok()
            .and_then(|index| threads.get(index).copied())
            .ok_or_else(|| CommandError::new(format!("no thread with index {index} (the process has {})", threads.len())))?;
        ctx.session().select_thread(thread)?;
        writeln!(ctx.out, "selected thread {index} (thread {})", thread.raw())?;
        Ok(Flow::Continue)
    }
}
