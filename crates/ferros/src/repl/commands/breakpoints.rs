//! Breakpoints: `break`, `delete`, `enable`, `disable`, `breakpoints`.

use std::fmt::Write as _;

use ferros_core::{BreakpointLocation, LogicalBreakpoint, LogicalBreakpointId, SourcePosition};

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args, parse_address, parse_number};
use crate::repl::Ctx;

/// Parse the argument of `break`: `*<address>` (or a bare number), `<file>:<line>`,
/// `<file>:<line>:<column>` (the column is ignored: breakpoints are per line), or a
/// function path.
///
/// A `file:line` form is recognised before a bare number, so a relative path that starts
/// with a digit (`2024/x.rs:3`) is a file, not an address.
pub fn parse_location(text: &str) -> Result<BreakpointLocation, CommandError>
{
    let text = text.trim();
    if text.is_empty() {
        return Err(CommandError::new("usage: break <file:line | function | *address>"));
    }
    if text.starts_with('*') {
        return parse_address(text).map(BreakpointLocation::Address);
    }
    if let Some((file, line)) = split_line(text) {
        // `file:line:column`: drop the column.
        let (file, line) = split_line(file).unwrap_or((file, line));
        let line = line
            .parse::<u32>()
            .map_err(|_| CommandError::new(format!("invalid line '{line}'")))?;
        return Ok(BreakpointLocation::Line {
            file: file.to_owned(),
            line,
        });
    }
    if text.starts_with(|c: char| c.is_ascii_digit()) {
        return parse_address(text).map(BreakpointLocation::Address);
    }
    Ok(BreakpointLocation::Function(text.to_owned()))
}

/// `text` split at its last lone `:` followed only by digits, as `(before, digits)`.
/// `a::b` splits into `a:` and `b`, which is a path, not a line.
fn split_line(text: &str) -> Option<(&str, &str)>
{
    let (file, line) = text.rsplit_once(':')?;
    (!file.is_empty() && !file.ends_with(':') && !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some((file, line))
}

/// `<function> at <file>:<line>`, or the parts of it that are known; empty when none is.
pub fn describe_position(position: &SourcePosition) -> String
{
    let mut text = String::new();
    if let Some(function) = &position.function {
        text.push_str(function);
    }
    if let Some(file) = &position.file {
        if !text.is_empty() {
            text.push_str(" at ");
        }
        text.push_str(file);
        if let Some(line) = position.line {
            let _ = write!(text, ":{line}");
        }
    }
    text
}

/// One line for a site: `<address>` followed by `: <position>` when that is known.
fn site_line(address: ferros_core::Address, position: &SourcePosition) -> String
{
    let described = describe_position(position);
    if described.is_empty() {
        format!("{:#x}", address.value())
    } else {
        format!("{:#x}: {described}", address.value())
    }
}

fn parse_id(command: &str, args: &str) -> Result<LogicalBreakpointId, CommandError>
{
    if args.is_empty() {
        return Err(CommandError::new(format!("usage: {command} <id>")));
    }
    Ok(LogicalBreakpointId::from_raw(parse_number(args)?))
}

fn find(ctx: &mut Ctx<'_>, id: LogicalBreakpointId) -> Option<LogicalBreakpoint>
{
    ctx.session()
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .cloned()
}

/// `break <file:line | function | *address>`: add a breakpoint.
pub struct Break;

impl ReplCommand for Break
{
    fn names(&self) -> &'static [&'static str]
    {
        &["break", "b"]
    }

    fn summary(&self) -> &'static str
    {
        "add a breakpoint: break <file:line | function | *address>"
    }

    fn help(&self) -> &'static str
    {
        "break <file:line | function | *address> (alias: b)\n\nAdd a breakpoint at:\n  <file>:<line>  a source \
         line; <file> is a full path or its last components (main.rs, src/main.rs). A line with no code uses the \
         next line that has code, and says so. A trailing :<column> is accepted and ignored.\n  <function>     a Rust function path (parse, my_crate::parse, \
         Counter::bump); the breakpoint is after the function's prologue.\n  *<address>     a runtime address, as \
         0x-prefixed hex or decimal.\nOne breakpoint can have several locations (generic instantiations, inlined \
         copies); it stops at each, and is listed, disabled and deleted as one. Prints 'breakpoint <id> at \
         <address>: <function> at <file>:<line>', or 'breakpoint <id>: <n> locations' followed by one line per \
         location. Source breakpoints are resolved again when the program is relaunched."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let location = parse_location(args)?;
        let id = ctx.session().set_breakpoint(location.clone())?;
        let breakpoint = find(ctx, id).ok_or_else(|| CommandError::new("the new breakpoint disappeared"))?;
        if let Some(reason) = &breakpoint.pending {
            writeln!(ctx.out, "breakpoint {} at {location} (pending: {reason})", id.raw())?;
            return Ok(Flow::Continue);
        }
        let note = match (&location, breakpoint.sites.first().and_then(|site| site.position.line)) {
            (BreakpointLocation::Line { line, .. }, Some(used)) if used != *line => {
                format!(" (line {line} has no code)")
            }
            _ => String::new(),
        };
        match breakpoint.sites.as_slice() {
            [site] => writeln!(
                ctx.out,
                "breakpoint {} at {}{note}",
                id.raw(),
                site_line(site.address, &site.position)
            )?,
            sites => {
                writeln!(ctx.out, "breakpoint {}: {} locations{note}", id.raw(), sites.len())?;
                for site in sites {
                    writeln!(ctx.out, "  {}", site_line(site.address, &site.position))?;
                }
            }
        }
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
        "delete <id> (alias: d)\n\nRemove the breakpoint with this id (see 'breakpoints') and every one of its \
         locations, restoring the original instructions."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let id = parse_id("delete", args)?;
        ctx.session().delete_breakpoint(id)?;
        writeln!(ctx.out, "deleted breakpoint {}", id.raw())?;
        Ok(Flow::Continue)
    }
}

/// `enable <id>`: re-arm a disabled breakpoint.
pub struct Enable;

impl ReplCommand for Enable
{
    fn names(&self) -> &'static [&'static str]
    {
        &["enable"]
    }

    fn summary(&self) -> &'static str
    {
        "re-arm a disabled breakpoint: enable <id>"
    }

    fn help(&self) -> &'static str
    {
        "enable <id>\n\nRe-arm every location of the breakpoint with this id. Enabling an enabled breakpoint does \
         nothing."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let id = parse_id("enable", args)?;
        ctx.session().enable_logical_breakpoint(id)?;
        writeln!(ctx.out, "enabled breakpoint {}", id.raw())?;
        Ok(Flow::Continue)
    }
}

/// `disable <id>`: keep a breakpoint but stop trapping at it.
pub struct Disable;

impl ReplCommand for Disable
{
    fn names(&self) -> &'static [&'static str]
    {
        &["disable"]
    }

    fn summary(&self) -> &'static str
    {
        "keep a breakpoint without stopping at it: disable <id>"
    }

    fn help(&self) -> &'static str
    {
        "disable <id>\n\nStop trapping at every location of the breakpoint with this id, keeping it in the list \
         ('enable' re-arms it). Disabling a disabled breakpoint does nothing."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let id = parse_id("disable", args)?;
        ctx.session().disable_logical_breakpoint(id)?;
        writeln!(ctx.out, "disabled breakpoint {}", id.raw())?;
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
        "list the breakpoints with their locations and hit counts"
    }

    fn help(&self) -> &'static str
    {
        "breakpoints\n\nList every breakpoint as '<id> <requested location> <enabled|disabled> hits=<count>', then, \
         for a breakpoint with several locations, one '  <address>: <function> at <file>:<line>' line each, or \
         '  pending: <reason>' for one that is not resolved. Prints 'no breakpoints' when there are none."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("breakpoints", args)?;
        let breakpoints = ctx.session().logical_breakpoints().to_vec();
        if breakpoints.is_empty() {
            writeln!(ctx.out, "no breakpoints")?;
        }
        for breakpoint in breakpoints {
            let state = if breakpoint.enabled { "enabled" } else { "disabled" };
            let head = format!("{} {}", breakpoint.id.raw(), breakpoint.location);
            writeln!(ctx.out, "{head} {state} hits={}", breakpoint.hit_count)?;
            if let Some(reason) = &breakpoint.pending {
                writeln!(ctx.out, "  pending: {reason}")?;
                continue;
            }
            // A single address location is already its own description.
            if matches!(breakpoint.location, BreakpointLocation::Address(_)) && breakpoint.sites.len() == 1 {
                continue;
            }
            for site in &breakpoint.sites {
                writeln!(ctx.out, "  {}", site_line(site.address, &site.position))?;
            }
        }
        Ok(Flow::Continue)
    }
}

#[cfg(test)]
mod tests
{
    use ferros_core::Address;

    use super::*;

    #[test]
    fn locations_parse()
    {
        assert_eq!(
            parse_location("*0x10"),
            Ok(BreakpointLocation::Address(Address::new(16)))
        );
        assert_eq!(parse_location("0x10"), Ok(BreakpointLocation::Address(Address::new(16))));
        assert_eq!(
            parse_location("src/main.rs:12"),
            Ok(BreakpointLocation::Line {
                file: "src/main.rs".into(),
                line: 12
            })
        );
        assert_eq!(
            parse_location("my_crate::parse"),
            Ok(BreakpointLocation::Function("my_crate::parse".into()))
        );
        assert_eq!(parse_location("c"), Ok(BreakpointLocation::Function("c".into())));
        assert_eq!(
            parse_location("<my_crate::Counter>::bump"),
            Ok(BreakpointLocation::Function("<my_crate::Counter>::bump".into()))
        );
        assert!(parse_location("").is_err());
        assert!(parse_location("*").is_err());
    }

    /// Issue #109: `file:line:column` sets the same breakpoint as `file:line`.
    #[test]
    fn a_column_suffix_is_ignored()
    {
        assert_eq!(parse_location("main.rs:12:5"), parse_location("main.rs:12"));
        assert_eq!(
            parse_location("src/main.rs:12:5"),
            Ok(BreakpointLocation::Line {
                file: "src/main.rs".into(),
                line: 12
            })
        );
        assert_eq!(
            parse_location("my_crate::parse:7:1"),
            Ok(BreakpointLocation::Line {
                file: "my_crate::parse".into(),
                line: 7
            })
        );
    }

    /// Issue #109: a relative path that starts with a digit is a file, not an address;
    /// a bare number is still an address.
    #[test]
    fn a_path_starting_with_a_digit_is_a_file()
    {
        assert_eq!(
            parse_location("2024/x.rs:3"),
            Ok(BreakpointLocation::Line {
                file: "2024/x.rs".into(),
                line: 3
            })
        );
        assert_eq!(
            parse_location("2024/x.rs:3:9"),
            Ok(BreakpointLocation::Line {
                file: "2024/x.rs".into(),
                line: 3
            })
        );
        assert_eq!(parse_location("4096"), Ok(BreakpointLocation::Address(Address::new(4096))));
        assert!(parse_location("2024/x.rs").is_err(), "no line: not a function, not an address");
    }

    #[test]
    fn positions_describe_what_is_known()
    {
        let full = SourcePosition {
            function: Some("app::main".into()),
            file: Some("/w/src/main.rs".into()),
            line: Some(3),
        };
        assert_eq!(describe_position(&full), "app::main at /w/src/main.rs:3");
        let name_only = SourcePosition {
            function: Some("app::main".into()),
            ..SourcePosition::default()
        };
        assert_eq!(describe_position(&name_only), "app::main");
        assert_eq!(describe_position(&SourcePosition::default()), "");
    }
}
