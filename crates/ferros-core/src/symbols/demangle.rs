//! Symbol demangling utilities.
//!
//! This module provides functions to demangle symbol names and detect their
//! programming language. It handles Rust, C++, and C symbols.
//!
//! ## Symbol Mangling
//!
//! Compilers "mangle" symbol names to encode type information and namespaces.
//! This module can demangle these names back to human-readable form:
//!
//! - **Rust**: Uses v0 mangling scheme (e.g., `_R...`)
//! - **C++**: Uses Itanium ABI mangling (e.g., `_Z...`)
//! - **C**: Typically unmangled (global symbols)
//!
//! ## Language Detection
//!
//! The module detects the language of a symbol by examining its mangling pattern:
//!
//! - Rust symbols: Start with `_R` or `_ZN`, or contain `::`
//! - C++ symbols: Start with `_Z` (Itanium mangling)
//! - C symbols: Everything else

use rustc_demangle::try_demangle;

use crate::error::DebuggerError;
use crate::types::{SymbolLanguage, SymbolName};

/// Create a `SymbolName` from a raw mangled symbol string.
///
/// This function attempts to demangle the symbol and detect its programming
/// language. It uses the `rustc_demangle` crate to demangle Rust symbols, and
/// heuristics to detect C++ and C symbols.
///
/// ## Parameters
///
/// - `raw`: The raw (mangled) symbol name from the binary
///
/// ## Returns
///
/// A `SymbolName` containing:
/// - The raw mangled name
/// - The demangled name (if demangling succeeded)
/// - The detected language (Rust, C++, C, or Unknown)
///
/// ## Example
///
/// ```rust,no_run
/// // Note: This function is internal to the crate.
/// // In practice, symbol names are created automatically during symbolication.
/// ```
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

/// Check if a type name represents a Rust trait object.
///
/// This function uses heuristics to detect trait objects by looking for the
/// `dyn` keyword in the type name. Trait objects in Rust are represented as
/// `dyn Trait` or `(dyn Trait)`.
///
/// ## Parameters
///
/// - `name`: The type name to check
///
/// ## Returns
///
/// `true` if the name appears to be a trait object, `false` otherwise.
///
/// ## Example
///
/// ```rust,no_run
/// // Note: This function is internal to the crate.
/// // Examples of trait object patterns:
/// // - "dyn std::fmt::Display"
/// // - "(dyn std::fmt::Display)"
/// // - "std::boxed::Box<dyn std::fmt::Display>"
/// // - "std::string::String" (not a trait object)
/// ```
pub(crate) fn is_trait_object(name: &str) -> bool
{
    let trimmed = name.trim();
    trimmed.starts_with("dyn ") || trimmed.starts_with("(dyn ") || trimmed.contains(" as dyn ") || trimmed.contains(" dyn ")
}

/// Map a gimli DWARF error to a `DebuggerError` with context.
///
/// This helper function wraps gimli errors with additional context about what
/// operation was being performed when the error occurred.
///
/// ## Parameters
///
/// - `context`: A description of what operation was being performed (e.g., "parsing .debug_info")
/// - `err`: The gimli error that occurred
///
/// ## Returns
///
/// A `DebuggerError::InvalidArgument` with a formatted message combining the
/// context and error details.
pub(crate) fn map_dwarf_error(context: &str, err: gimli::Error) -> DebuggerError
{
    DebuggerError::InvalidArgument(format!("{context}: {err}"))
}
