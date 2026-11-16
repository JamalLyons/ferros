//! `help`.

use super::{COMMANDS, CommandError, CommandResult, Flow, ReplCommand, find};
use crate::repl::Ctx;

/// `help [command]`: the command list, or one command's details.
pub struct Help;

impl ReplCommand for Help
{
    fn names(&self) -> &'static [&'static str]
    {
        &["help", "h", "?"]
    }

    fn summary(&self) -> &'static str
    {
        "list the commands, or show one: help <command>"
    }

    fn help(&self) -> &'static str
    {
        "help [command] (aliases: h, ?)\n\nWithout an argument, list every command. With one, show its usage."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        if args.is_empty() {
            let width = COMMANDS.iter().map(|command| command.names()[0].len()).max().unwrap_or(0);
            for command in COMMANDS {
                writeln!(ctx.out, "{:<width$}  {}", command.names()[0], command.summary())?;
            }
            return Ok(Flow::Continue);
        }
        let command = find(args).ok_or_else(|| CommandError::new(format!("unknown command '{args}' (try 'help')")))?;
        writeln!(ctx.out, "{}", command.help())?;
        Ok(Flow::Continue)
    }
}
