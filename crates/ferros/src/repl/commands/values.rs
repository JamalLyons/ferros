//! Raw variable inspection.

use super::{CommandResult, Flow, ReplCommand, no_args};
use crate::repl::Ctx;

/// `locals`: show the selected frame's variables.
pub struct Locals;

impl ReplCommand for Locals
{
    fn names(&self) -> &'static [&'static str]
    {
        &["locals"]
    }

    fn summary(&self) -> &'static str
    {
        "show the selected frame's arguments and locals as raw bytes"
    }

    fn help(&self) -> &'static str
    {
        "locals\n\nList the arguments and local variables visible in the selected frame. Each entry shows its name, type, \
         and raw bytes in memory order, or why its value is unavailable."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("locals", args)?;
        let variables = ctx.session().selected_frame_variables()?;
        if variables.is_empty() {
            writeln!(ctx.out, "No variables in scope.")?;
        }
        for variable in variables {
            let ty = variable.type_name.as_deref().unwrap_or("?");
            write!(ctx.out, "{}: {ty} = ", variable.name)?;
            match variable.value.bytes {
                Ok(bytes) => {
                    write!(ctx.out, "0x")?;
                    for byte in bytes {
                        write!(ctx.out, "{byte:02x}")?;
                    }
                    writeln!(ctx.out)?;
                }
                Err(reason) => writeln!(ctx.out, "unavailable: {reason}")?,
            }
        }
        Ok(Flow::Continue)
    }
}
