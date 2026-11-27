//! Type-specific renderers for values whose logical contents differ from their fields.

mod enums;

use super::format::{ValueFormatter, ValueView};
use super::types::{Type, TypeTable};

/// A type-specific formatter. Add each family as a module and register it in [`PRINTERS`].
pub trait PrettyPrinter: Sync
{
    /// Match the name or layout of a type. The first matching printer wins.
    fn matches(&self, ty: &Type, types: &TypeTable) -> bool;

    /// Render the value. Use the formatter to read memory and render children with
    /// the current limits and cycle guard.
    fn format(&self, value: ValueView<'_>, formatter: &mut ValueFormatter<'_>) -> String;
}

/// Built-in printers in matching order. Unmatched types use the generic formatter.
pub static PRINTERS: &[&dyn PrettyPrinter] = &[&enums::EnumPrinter];
