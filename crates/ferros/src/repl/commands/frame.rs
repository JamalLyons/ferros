//! Frame selection: `frame`, `up`, `down`.

use ferros_core::DebuggerError;

use super::stack::format_frame;
use super::{CommandError, CommandResult, Flow, ReplCommand, parse_number};
use crate::repl::Ctx;

/// `frame [n]`: print the selected frame, or select frame `n`.
pub struct Frame;

/// `up [n]`: select an older frame.
pub struct Up;

/// `down [n]`: select a newer frame.
pub struct Down;

impl ReplCommand for Frame
{
    fn names(&self) -> &'static [&'static str]
    {
        &["frame"]
    }

    fn summary(&self) -> &'static str
    {
        "show or select a stack frame: frame [n]"
    }

    fn help(&self) -> &'static str
    {
        "frame [n]\n\nWith no argument, print the selected frame. With n, select that frame (0 is the innermost; inline \
         frames count, as in 'backtrace') and print it. The line matches one 'backtrace' line, including the '*' that marks \
         the selected frame. The selection returns to frame 0 after a stop, after continue or a step, and after 'thread'."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        if args.is_empty() {
            return print_selected(ctx);
        }
        let frame = ctx.session().select_frame(parse_index(args)?)?;
        writeln!(ctx.out, "{}", format_frame(&frame, true))?;
        Ok(Flow::Continue)
    }
}

impl ReplCommand for Up
{
    fn names(&self) -> &'static [&'static str]
    {
        &["up"]
    }

    fn summary(&self) -> &'static str
    {
        "select an older frame: up [n]"
    }

    fn help(&self) -> &'static str
    {
        "up [n]\n\nSelect the frame n callers out from the selected one (default 1) and print it. An n that would pass the \
         oldest frame is an error and leaves the selection unchanged."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        move_frame(ctx, Direction::Up, parse_steps(args)?)
    }
}

impl ReplCommand for Down
{
    fn names(&self) -> &'static [&'static str]
    {
        &["down"]
    }

    fn summary(&self) -> &'static str
    {
        "select a newer frame: down [n]"
    }

    fn help(&self) -> &'static str
    {
        "down [n]\n\nSelect the frame n calls in from the selected one (default 1) and print it. An n that would pass the \
         newest frame (frame 0) is an error and leaves the selection unchanged."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        move_frame(ctx, Direction::Down, parse_steps(args)?)
    }
}

#[derive(Clone, Copy)]
enum Direction
{
    Up,
    Down,
}

fn move_frame(ctx: &mut Ctx<'_>, direction: Direction, count: usize) -> CommandResult
{
    let current = ctx.session().selected_frame();
    let target = match direction {
        Direction::Up => match current.checked_add(count) {
            Some(target) => target,
            None => return end_of_stack_if_stopped(ctx, "already at the oldest frame"),
        },
        Direction::Down => match current.checked_sub(count) {
            Some(target) => target,
            None => return end_of_stack_if_stopped(ctx, "already at the newest frame"),
        },
    };
    let frame = match ctx.session().select_frame(target) {
        Ok(frame) => frame,
        Err(error) => return Err(end_of_stack(error, direction)),
    };
    writeln!(ctx.out, "{}", format_frame(&frame, true))?;
    Ok(Flow::Continue)
}

/// `at_end` only when a stopped process really is at that end of the stack. A missing
/// or running process reports that error instead.
fn end_of_stack_if_stopped(ctx: &mut Ctx<'_>, at_end: &str) -> CommandResult
{
    ctx.session().selected_stack_frame()?;
    Err(CommandError::new(at_end))
}

fn end_of_stack(error: DebuggerError, direction: Direction) -> CommandError
{
    let at_end = match direction {
        Direction::Up => "already at the oldest frame",
        Direction::Down => "already at the newest frame",
    };
    match error {
        DebuggerError::InvalidArgument(message) if message.starts_with("no frame ") => CommandError::new(at_end),
        other => other.into(),
    }
}

fn print_selected(ctx: &mut Ctx<'_>) -> CommandResult
{
    let frame = ctx.session().selected_stack_frame()?;
    writeln!(ctx.out, "{}", format_frame(&frame, true))?;
    Ok(Flow::Continue)
}

fn parse_index(args: &str) -> Result<usize, CommandError>
{
    usize::try_from(parse_number(args)?).map_err(|_| CommandError::new("the frame index is too large"))
}

fn parse_steps(args: &str) -> Result<usize, CommandError>
{
    if args.is_empty() {
        return Ok(1);
    }
    usize::try_from(parse_number(args)?)
        .ok()
        .filter(|&count| count > 0)
        .ok_or_else(|| CommandError::new("the frame count must be at least 1"))
}
