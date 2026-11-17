//! The command registry.
//!
//! Every command is a unit struct implementing [`ReplCommand`] in one of the modules
//! below, grouped by area. [`COMMANDS`] lists them in the order `help` prints them.
//!
//! To add a command, implement [`ReplCommand`] in a new file of this directory (or an
//! existing one) and add one line to the `registry!` invocation at the bottom of this
//! file: `module: [Command]`.

use std::fmt;

use ferros_core::{Address, DebuggerError};

use super::Ctx;

/// What the loop does after a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow
{
    /// Read the next command.
    Continue,
    /// Stop reading commands and end the session.
    Quit,
}

/// Why a command failed. Printed as `error: <message>` on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError(pub String);

impl CommandError
{
    /// An error with `message`.
    pub fn new(message: impl Into<String>) -> Self
    {
        Self(message.into())
    }
}

impl fmt::Display for CommandError
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        f.write_str(&self.0)
    }
}

impl From<DebuggerError> for CommandError
{
    fn from(error: DebuggerError) -> Self
    {
        Self(error.to_string())
    }
}

impl From<std::io::Error> for CommandError
{
    fn from(error: std::io::Error) -> Self
    {
        Self(format!("output error: {error}"))
    }
}

/// Result of running one command.
pub type CommandResult = Result<Flow, CommandError>;

/// One REPL command.
pub trait ReplCommand: Sync
{
    /// The command's name followed by its aliases. The first name is shown by `help`.
    fn names(&self) -> &'static [&'static str];

    /// One line for the `help` overview.
    fn summary(&self) -> &'static str;

    /// Usage and details for `help <command>`.
    fn help(&self) -> &'static str;

    /// Run the command. `args` is the rest of the line after the name, trimmed.
    ///
    /// # Errors
    ///
    /// A [`CommandError`] the loop prints as `error: <message>`; the session continues.
    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult;
}

/// The command named `name` (or with that alias), if any.
pub fn find(name: &str) -> Option<&'static dyn ReplCommand>
{
    COMMANDS.iter().copied().find(|command| command.names().contains(&name))
}

/// Fail when a command that takes no arguments was given some.
pub fn no_args(name: &str, args: &str) -> Result<(), CommandError>
{
    if args.is_empty() {
        Ok(())
    } else {
        Err(CommandError::new(format!("'{name}' takes no arguments")))
    }
}

/// Parse an unsigned number: `0x`-prefixed hexadecimal or decimal. `_` separators are
/// allowed.
pub fn parse_number(text: &str) -> Result<u64, CommandError>
{
    let cleaned = text.trim().replace('_', "");
    let parsed = if let Some(hex) = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        cleaned.parse::<u64>()
    };
    parsed.map_err(|_| CommandError::new(format!("invalid number '{text}' (use decimal or 0x-prefixed hex)")))
}

/// Parse an address: a number as for [`parse_number`], optionally prefixed with `*`.
pub fn parse_address(text: &str) -> Result<Address, CommandError>
{
    let text = text.trim();
    let bare = text.strip_prefix('*').unwrap_or(text).trim();
    if bare.is_empty() {
        return Err(CommandError::new("missing address"));
    }
    parse_number(bare).map(Address::from)
}

/// Declare the command modules and build [`COMMANDS`] from them.
macro_rules! registry {
    ($($module:ident: [$($command:ident),* $(,)?]),* $(,)?) => {
        $(mod $module;)*

        /// Every command, in the order `help` lists them.
        pub static COMMANDS: &[&dyn ReplCommand] = &[$($(&$module::$command),*),*];
    };
}

pub use self::breakpoints::describe_position;

registry! {
    process: [Run, Continue, Interrupt, Kill, Status, Quit],
    step: [Next, Step, Finish, StepInstruction],
    breakpoints: [Break, Delete, Enable, Disable, Breakpoints],
    inspect: [Registers, Memory],
    threads: [Threads, Thread],
    stack: [Backtrace],
    images: [Images],
    help: [Help],
}
