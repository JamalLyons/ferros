//! State inspection: `registers`, `memory`.

use super::{CommandError, CommandResult, Flow, ReplCommand, no_args, parse_address, parse_number};
use crate::repl::Ctx;
use crate::repl::output::{DEFAULT_MEMORY_LEN, format_registers, hexdump};

/// Largest `memory` read, so a typo cannot ask for gigabytes.
const MAX_MEMORY_LEN: u64 = 1 << 20;

/// `registers`: the selected thread's registers.
pub struct Registers;

impl ReplCommand for Registers
{
    fn names(&self) -> &'static [&'static str]
    {
        &["registers", "regs"]
    }

    fn summary(&self) -> &'static str
    {
        "show the selected thread's registers"
    }

    fn help(&self) -> &'static str
    {
        "registers (alias: regs)\n\nPrint pc, sp, fp, lr, cpsr and x0..x28 of the selected thread, one per \
         line as '<name> <value>'."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("registers", args)?;
        let registers = ctx.session().read_registers()?;
        write!(ctx.out, "{}", format_registers(&registers))?;
        Ok(Flow::Continue)
    }
}

/// `memory <address> [len]`: a hexdump.
pub struct Memory;

impl ReplCommand for Memory
{
    fn names(&self) -> &'static [&'static str]
    {
        &["memory", "x"]
    }

    fn summary(&self) -> &'static str
    {
        "hexdump memory: memory <address> [len]"
    }

    fn help(&self) -> &'static str
    {
        "memory <address> [len] (alias: x)\n\nRead len bytes (default 64) at address and print them 16 per row \
         with their ASCII text. Numbers are 0x-prefixed hex or decimal; the address may start with '*'. \
         Breakpoint traps are hidden: the original bytes are shown."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        let mut words = args.split_whitespace();
        let address = parse_address(words.next().ok_or_else(|| CommandError::new("usage: memory <address> [len]"))?)?;
        let len = match words.next() {
            Some(text) => parse_number(text)?,
            None => DEFAULT_MEMORY_LEN as u64,
        };
        if words.next().is_some() {
            return Err(CommandError::new("usage: memory <address> [len]"));
        }
        if len == 0 || len > MAX_MEMORY_LEN {
            return Err(CommandError::new(format!("length must be between 1 and {MAX_MEMORY_LEN}")));
        }
        let len = usize::try_from(len).map_err(|_| CommandError::new("length too large"))?;
        let bytes = ctx.session().read_memory(address, len)?;
        write!(ctx.out, "{}", hexdump(address, &bytes))?;
        Ok(Flow::Continue)
    }
}
