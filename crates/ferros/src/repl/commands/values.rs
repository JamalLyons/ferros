//! Inspect the values visible in the selected frame.

use ferros_core::values::format::FormatOptions;
use ferros_core::values::path::Path;
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

/// `print <path>`: show one of the selected frame's variables, or a value inside it.
pub struct Print;

impl ReplCommand for Print
{
    fn names(&self) -> &'static [&'static str]
    {
        &["print"]
    }

    fn summary(&self) -> &'static str
    {
        "show a variable, or a field, element or pointee inside one"
    }

    fn help(&self) -> &'static str
    {
        "print <path>\n\nShow the type and value of an argument or local variable in the selected frame, or of a \
         value reached from it:\n  name        the variable\n  name.field  a field of a struct or of an enum's \
         active variant (name.0 for a tuple field)\n  name[n]     an element of an array, slice or Vec\n  *name       the value a reference, Box or \
         raw pointer points to\n  (path)      grouping, as in (*name).field\n\nField access and indexing look \
         through references and boxes. Function calls and operators are not supported."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        if args.is_empty() {
            return Err(CommandError::new("usage: print <path>"));
        }
        let path = Path::parse(args).map_err(|error| CommandError::new(format!("invalid path '{args}': {error}")))?;
        let root = path.root();
        let variables = ctx.session().selected_frame_variables()?;
        // Outer scopes precede inner scopes, so the last declaration wins when shadowed.
        let variable = variables
            .iter()
            .rfind(|variable| variable.name == root)
            .ok_or_else(|| CommandError::new(format!("no variable named '{root}' in the selected frame")))?;
        let (ty, text) = ctx
            .session()
            .format_path(variable, &path, FormatOptions::default())
            .map_err(|error| CommandError::new(format!("cannot evaluate '{path}': {error}")))?;
        let ty = ty.as_deref().unwrap_or("?");
        writeln!(ctx.out, "{path}: {ty} = {text}")?;
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
