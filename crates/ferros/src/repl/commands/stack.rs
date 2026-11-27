//! Stack: `backtrace`.

use ferros_core::{FrameKind, StackFrame};

use super::{CommandError, CommandResult, Flow, ReplCommand, parse_number};
use crate::repl::Ctx;

/// Frames printed when no limit is given.
const DEFAULT_FRAMES: usize = 256;

/// `backtrace [n]`: the selected thread's stack.
pub struct Backtrace;

impl ReplCommand for Backtrace
{
    fn names(&self) -> &'static [&'static str]
    {
        &["backtrace", "bt"]
    }

    fn summary(&self) -> &'static str
    {
        "show the selected thread's stack: backtrace [n]"
    }

    fn help(&self) -> &'static str
    {
        "backtrace [n] (alias: bt)\n\nPrint the selected thread's frames, innermost first, at most n (default 256), one per \
         line as '#<index> <pc> in <function> at <file>:<line>'. The selected frame has '*' after its address. A caller's \
         pc is its return address; its line is the call site. Functions inlined into a frame are listed before it and \
         marked '[inlined]'. A frame whose code has no debug info is named from its image's symbol table (the nearest \
         symbol before its pc) and printed with no location; a frame with neither prints '??'."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let limit = if args.is_empty() {
            DEFAULT_FRAMES
        } else {
            usize::try_from(parse_number(args)?)
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| CommandError::new("the frame count must be at least 1"))?
        };
        let selected = ctx.session().selected_frame();
        let frames = ctx.session().stack_trace(limit)?;
        for frame in &frames {
            writeln!(ctx.out, "{}", format_frame(frame, frame.index == selected))?;
        }
        Ok(Flow::Continue)
    }
}

/// `#<index> <pc>[ *] in <function> at <file>:<line>[ [inlined]]`.
///
/// `*` after the address marks the selected frame.
pub(super) fn format_frame(frame: &StackFrame, selected: bool) -> String
{
    let function = frame
        .symbol
        .as_ref()
        .map_or_else(|| "??".to_owned(), |symbol| symbol.display_name().to_owned());
    // Built in pieces: one `format!` with the name as well trips `large_stack_arrays`.
    let mut line = format!("#{} {:#018x}", frame.index, frame.pc.value());
    if selected {
        line.push_str(" *");
    }
    line.push_str(" in ");
    line.push_str(&function);
    if let Some(location) = &frame.location {
        line.push_str(" at ");
        line.push_str(&location.file);
        if let Some(number) = location.line {
            line.push(':');
            line.push_str(&number.to_string());
        }
    }
    if matches!(frame.kind, FrameKind::Inlined { .. }) {
        line.push_str(" [inlined]");
    }
    line
}

#[cfg(test)]
mod tests
{
    use ferros_core::{
        Address, FrameId, FrameKind, FrameStatus, SourceLocation, StackFrame, SymbolLanguage, SymbolName, ThreadId,
    };

    use super::format_frame;

    fn show(frame: &StackFrame) -> String
    {
        format_frame(frame, false)
    }

    fn frame(kind: FrameKind, name: Option<&str>, line: Option<u32>) -> StackFrame
    {
        let thread = ThreadId::from(1);
        StackFrame {
            id: FrameId::new(thread, 0, 0, Address::new(0x1000), Address::new(0x2000)),
            thread,
            index: 3,
            kind,
            pc: Address::new(0x1000),
            sp: Address::new(0x2000),
            fp: Address::ZERO,
            return_address: None,
            symbol: name.map(|name| SymbolName::from_debug_info(name.to_owned(), None, SymbolLanguage::Rust)),
            location: line.map(|line| SourceLocation {
                file: "src/main.rs".to_owned(),
                line: Some(line),
                column: None,
            }),
            parameters: Vec::new(),
            status: FrameStatus::Complete,
        }
    }

    #[test]
    fn frames_format_with_location_and_inlined_marker()
    {
        let main = frame(FrameKind::Physical, Some("app::main"), Some(7));
        assert_eq!(show(&main), "#3 0x0000000000001000 in app::main at src/main.rs:7");
        assert_eq!(
            format_frame(&main, true),
            "#3 0x0000000000001000 * in app::main at src/main.rs:7"
        );
        let physical = FrameId::new(ThreadId::from(1), 0, 0, Address::new(0x1000), Address::new(0x2000));
        assert_eq!(
            show(&frame(FrameKind::Inlined { physical, depth: 0 }, Some("app::f"), Some(2))),
            "#3 0x0000000000001000 in app::f at src/main.rs:2 [inlined]"
        );
        assert_eq!(show(&frame(FrameKind::Physical, None, None)), "#3 0x0000000000001000 in ??");
        // A name from the symbol table only has no location to print.
        assert_eq!(
            show(&frame(FrameKind::Physical, Some("_pthread_start"), None)),
            "#3 0x0000000000001000 in _pthread_start"
        );
    }
}
