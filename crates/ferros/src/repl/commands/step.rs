//! Source and instruction stepping.

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


/// `next`: advance over calls.
pub struct Next;
/// `step`: advance into calls with debug information.
pub struct Step;
/// `finish`: return to the caller.
pub struct Finish;

macro_rules! source_command {
    ($command:ident, $method:ident, $names:expr, $summary:literal, $help:literal) => {
        impl ReplCommand for $command {
            fn names(&self) -> &'static [&'static str] { $names }
            fn summary(&self) -> &'static str { $summary }
            fn help(&self) -> &'static str { $help }
            fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult {
                no_args(self.names()[0], args)?;
                ctx.out.flush()?;
                ctx.interrupt.store(false, std::sync::atomic::Ordering::SeqCst);
                ctx.session().$method()?;
                ctx.debugger.at_entry = false;
                let event = crate::repl::interrupt::wait_for_stop(&mut ctx.debugger.session, ctx.interrupt)?;
                ctx.print_stop(&event)?;
                Ok(Flow::Continue)
            }
        }
    };
}

source_command!(Next, step_over, &["next", "n"], "advance one source line, over calls",
    "next (alias: n)\n\nAdvance one source line, stepping over calls. Other threads stay suspended. A user breakpoint or other stop ends the step.");
source_command!(Step, step_into, &["step", "s"], "advance one source line, into calls",
    "step (alias: s)\n\nEnter calls with line information, stopping after the prologue. Skip calls without line information. Other threads stay suspended.");
source_command!(Finish, step_out, &["finish"], "run to the caller of the current frame",
    "finish\n\nRun to the caller of the selected thread's current frame. Other threads stay suspended. A user breakpoint or other stop ends the step.");
