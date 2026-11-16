//! Breakpoints: `break`, `delete`, `breakpoints`.

use ferros_core::{BreakpointId, BreakpointRequest};

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args, parse_address, parse_number};
use crate::repl::Ctx;

/// `break *<address>`: add a software breakpoint.
pub struct Break;

impl ReplCommand for Break
{
    fn names(&self) -> &'static [&'static str]
    {
        &["break", "b"]
    }

    fn summary(&self) -> &'static str
    {
        "add a breakpoint at an address: break *<address>"
    }

    fn help(&self) -> &'static str
    {
        "break *<address> (alias: b)\n\nAdd a software breakpoint at a runtime address, given as 0x-prefixed \
         hex or decimal (the '*' is optional). Prints 'breakpoint <id> at <address>'. Use 'images' to find \
         where the program is loaded."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        if args.is_empty() {
            return Err(CommandError::new("usage: break *<address>"));
        }
        let address = parse_address(args)?;
        let id = ctx.session().add_breakpoint(BreakpointRequest::Software { address })?;
        writeln!(ctx.out, "breakpoint {} at {:#x}", id.raw(), address.value())?;
        Ok(Flow::Continue)
    }
}

/// `delete <id>`: remove a breakpoint.
pub struct Delete;

impl ReplCommand for Delete
{
    fn names(&self) -> &'static [&'static str]
    {
        &["delete", "d"]
    }

    fn summary(&self) -> &'static str
    {
        "remove a breakpoint: delete <id>"
    }

    fn help(&self) -> &'static str
    {
        "delete <id> (alias: d)\n\nRemove the breakpoint with this id (see 'breakpoints') and restore the \
         original instruction."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        if args.is_empty() {
            return Err(CommandError::new("usage: delete <id>"));
        }
        let id = BreakpointId::from_raw(parse_number(args)?);
        ctx.session().remove_breakpoint(id)?;
        writeln!(ctx.out, "deleted breakpoint {}", id.raw())?;
        Ok(Flow::Continue)
    }
}

/// `breakpoints`: list the breakpoints.
pub struct Breakpoints;

impl ReplCommand for Breakpoints
{
    fn names(&self) -> &'static [&'static str]
    {
        &["breakpoints", "info-breakpoints"]
    }

    fn summary(&self) -> &'static str
    {
        "list the breakpoints with their hit counts"
    }

    fn help(&self) -> &'static str
    {
        "breakpoints\n\nList every breakpoint as '<id> <address> <enabled|disabled> hits=<count>', or 'no \
         breakpoints'."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("breakpoints", args)?;
        let breakpoints = ctx.session().breakpoints();
        if breakpoints.is_empty() {
            writeln!(ctx.out, "no breakpoints")?;
        }
        for info in breakpoints {
            let state = if info.enabled { "enabled" } else { "disabled" };
            let location = format!("{} {:#x}", info.id.raw(), info.address.value());
            writeln!(ctx.out, "{location} {state} hits={}", info.hit_count)?;
        }
        Ok(Flow::Continue)
    }
}
