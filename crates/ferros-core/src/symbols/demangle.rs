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

use crate::types::{SymbolLanguage, SymbolName, SymbolSource};

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
pub(crate) fn make_symbol_name(raw: String, source: SymbolSource) -> SymbolName
{
    // The alternate form leaves out crate disambiguators (`[e9e28b783c7f9d4c]`) and legacy
    // `::h<hash>` suffixes, as the function index does.
    let demangled = try_demangle(&raw).ok().map(|d| format!("{d:#}"));
    let language = if raw.starts_with("_R") || raw.starts_with("_ZN") || raw.contains("::") {
        SymbolLanguage::Rust
    } else if raw.starts_with("_Z") {
        SymbolLanguage::Cpp
    } else {
        SymbolLanguage::Unknown
    };

    SymbolName::new(raw, demangled, language, source)
}
