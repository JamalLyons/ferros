//! Statics of the main executable: `globals`.

use ferros_core::values::format::FormatOptions;

use super::{CommandResult, Flow, ReplCommand};
use crate::repl::Ctx;

/// `globals [filter]`: list the main executable's statics with their values.
pub struct Globals;

impl ReplCommand for Globals
{
    fn names(&self) -> &'static [&'static str]
    {
        &["globals"]
    }

    fn summary(&self) -> &'static str
    {
        "list the main executable's statics and their values"
    }

    fn help(&self) -> &'static str
    {
        "globals [filter]\n\nList the statics of the main executable, sorted by qualified name, as \
         '<name>: <type> = <value> @ <address>', where the address is the static's runtime address in the \
         loaded image. With a filter, list only the statics whose qualified name contains it, such as the \
         crate or module name. Statics that share a name (such as two in separate blocks of one function) are \
         listed once each, in address order. Thread-locals are not listed. 'print <name>' shows one static."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let filter = (!args.is_empty()).then_some(args);
        let statics = ctx.session().statics(filter)?;
        if statics.is_empty() {
            match filter {
                Some(filter) => writeln!(ctx.out, "No statics matching '{filter}'.")?,
                None => writeln!(ctx.out, "No statics.")?,
            }
        }
        for found in &statics {
            let (ty, text) = ctx.session().format_variable(&found.variable, FormatOptions::default());
            // Written in pieces: one `writeln!` with every argument trips `large_stack_arrays`.
            write!(ctx.out, "{}: {}", found.variable.name, ty.as_deref().unwrap_or("?"))?;
            write!(ctx.out, " = {text}")?;
            if let Some(address) = found.address {
                write!(ctx.out, " @ {address}")?;
            }
            writeln!(ctx.out)?;
        }
        Ok(Flow::Continue)
    }
}
