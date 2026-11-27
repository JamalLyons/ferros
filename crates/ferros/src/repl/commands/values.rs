//! Inspect the values visible in the selected frame.

use ferros_core::values::format::FormatOptions;
use ferros_core::values::scope::Variable;

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args};
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
        "show the selected frame's arguments and locals"
    }

    fn help(&self) -> &'static str
    {
        "locals\n\nList the arguments and local variables visible in the selected frame with their types and values."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("locals", args)?;
        let variables = ctx.session().selected_frame_variables()?;
        if variables.is_empty() {
            writeln!(ctx.out, "No variables in scope.")?;
        }
        for variable in &variables {
            write_variable(ctx, variable)?;
        }
        Ok(Flow::Continue)
    }
}

/// `print <name>`: show one of the selected frame's variables.
pub struct Print;

impl ReplCommand for Print
{
    fn names(&self) -> &'static [&'static str]
    {
        &["print"]
    }

    fn summary(&self) -> &'static str
    {
        "show one argument or local variable by name"
    }

    fn help(&self) -> &'static str
    {
        "print <name>\n\nShow the type and value of an argument or local variable in the selected frame."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        if args.is_empty() || args.contains(char::is_whitespace) {
            return Err(CommandError::new("usage: print <name>"));
        }
        let variables = ctx.session().selected_frame_variables()?;
        // Outer scopes precede inner scopes, so the last declaration wins when shadowed.
        let variable = variables
            .iter()
            .rfind(|variable| variable.name == args)
            .ok_or_else(|| CommandError::new(format!("no variable named '{args}' in the selected frame")))?;
        write_variable(ctx, variable)?;
        Ok(Flow::Continue)
    }
}

fn write_variable(ctx: &mut Ctx<'_>, variable: &Variable) -> Result<(), CommandError>
{
    let (ty, text) = ctx.session().format_variable(variable, FormatOptions::default());
    let ty = ty.as_deref().unwrap_or("?");
    writeln!(ctx.out, "{}: {ty} = {text}", variable.name)?;
    Ok(())
}
