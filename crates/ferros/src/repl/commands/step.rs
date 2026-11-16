//! Instruction stepping: `stepi`.

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args};
use crate::repl::Ctx;

/// `stepi`: execute one instruction of the selected thread.
pub struct StepInstruction;

impl ReplCommand for StepInstruction
{
    fn names(&self) -> &'static [&'static str]
    {
        &["stepi", "si"]
    }

    fn summary(&self) -> &'static str
    {
        "execute one instruction of the selected thread"
    }

    fn help(&self) -> &'static str
    {
        "stepi (alias: si)\n\nExecute one instruction of the selected thread while the other threads stay \
         suspended, then print the stop: 'stopped: step at <pc> (thread <tid>)', or another stop that \
         happened first."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("stepi", args)?;
        let thread = ctx
            .session()
            .selected_thread()
            .ok_or_else(|| CommandError::new("no thread is selected"))?;
        ctx.resume_and_wait(Some(thread))?;
        Ok(Flow::Continue)
    }
}
