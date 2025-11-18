//! Symbol demangling utilities.

use rustc_demangle::try_demangle;

use crate::error::DebuggerError;
use crate::types::{SymbolLanguage, SymbolName};

/// Create a SymbolName from a raw mangled symbol string.
///
/// Attempts to demangle the symbol and detect its language (Rust, C++, C, Unknown).
pub(crate) fn make_symbol_name(raw: String) -> SymbolName
{
    let demangled = try_demangle(&raw).ok().map(|d| d.to_string());
    let language = if raw.starts_with("_R") || raw.starts_with("_ZN") || raw.contains("::") {
        SymbolLanguage::Rust
    } else if raw.starts_with("_Z") {
        SymbolLanguage::Cpp
    } else {
        SymbolLanguage::Unknown
    };

    SymbolName::new(raw, demangled, language)
}

/// Check if a type name represents a trait object.
pub(crate) fn is_trait_object(name: &str) -> bool
{
    let trimmed = name.trim();
    trimmed.starts_with("dyn ") || trimmed.starts_with("(dyn ") || trimmed.contains(" as dyn ") || trimmed.contains(" dyn ")
}

/// Map a gimli DWARF error to a DebuggerError with context.
pub(crate) fn map_dwarf_error(context: &str, err: gimli::Error) -> DebuggerError
{
    DebuggerError::InvalidArgument(format!("{context}: {err}"))
}
