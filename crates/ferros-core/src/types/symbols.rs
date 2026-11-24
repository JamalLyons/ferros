//! Symbol and source location types.

use std::fmt;

/// Function parameter information extracted from DWARF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionParameter
{
    /// Parameter name (if available in DWARF).
    pub name: Option<String>,
    /// Parameter type name (if available).
    pub type_name: Option<String>,
    /// Location where the parameter is stored (register, memory offset, etc.).
    pub location: Option<String>,
}

impl FunctionParameter
{
    /// Create a new function parameter with just a name.
    pub fn new(name: impl Into<String>) -> Self
    {
        Self {
            name: Some(name.into()),
            type_name: None,
            location: None,
        }
    }

    /// Create a function parameter with name and type.
    pub fn with_type(name: impl Into<String>, type_name: impl Into<String>) -> Self
    {
        Self {
            name: Some(name.into()),
            type_name: Some(type_name.into()),
            location: None,
        }
    }
}

/// Programming language associated with a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolLanguage
{
    /// Rust symbol (detected via mangling or namespace patterns).
    Rust,
    /// C++ symbol (Itanium mangling without Rust extensions).
    Cpp,
    /// C symbol or unmangled global.
    C,
    /// Unknown or mixed language.
    Unknown,
}

impl fmt::Display for SymbolLanguage
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        let label = match self {
            SymbolLanguage::Rust => "rust",
            SymbolLanguage::Cpp => "c++",
            SymbolLanguage::C => "c",
            SymbolLanguage::Unknown => "unknown",
        };
        write!(f, "{label}")
    }
}

/// Where a [`SymbolName`] came from. Every constructor of [`SymbolName`] names it; there
/// is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSource
{
    /// Debug information (DWARF): the name of the function or inlined call, usually with
    /// a source location.
    DebugInfo,
    /// Only the image's symbol table (read by the session's
    /// [`ObjectFormat`](crate::objfile::ObjectFormat)), for code without DWARF: the
    /// nearest symbol at or before the address. Such a name has no source location. A
    /// stripped image, or a table without local symbols, can name a pc after an earlier
    /// function when the containing one has no symbol of its own.
    SymbolTable,
}

/// A function or type name with demangling metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolName
{
    raw: String,
    demangled: Option<String>,
    language: SymbolLanguage,
    source: SymbolSource,
}

impl SymbolName
{
    /// Construct from a raw linkage name, stating where it came from.
    pub fn new(raw: String, demangled: Option<String>, language: SymbolLanguage, source: SymbolSource) -> Self
    {
        Self {
            raw,
            demangled,
            language,
            source,
        }
    }

    /// A name from debug information (DWARF).
    pub fn from_debug_info(raw: String, demangled: Option<String>, language: SymbolLanguage) -> Self
    {
        Self::new(raw, demangled, language, SymbolSource::DebugInfo)
    }

    /// A name from an image's symbol table only (see [`SymbolSource::SymbolTable`]).
    pub fn from_symbol_table(raw: String, demangled: Option<String>, language: SymbolLanguage) -> Self
    {
        Self::new(raw, demangled, language, SymbolSource::SymbolTable)
    }

    /// Where the name came from, as stated when it was constructed.
    pub fn source(&self) -> SymbolSource
    {
        self.source
    }

    /// Raw (mangled) name emitted in the object file.
    pub fn raw(&self) -> &str
    {
        &self.raw
    }

    /// Demangled human-friendly name if available.
    pub fn demangled(&self) -> Option<&str>
    {
        self.demangled.as_deref()
    }

    /// Preferred presentation (demangled fallback to raw).
    pub fn display_name(&self) -> &str
    {
        self.demangled.as_deref().unwrap_or(&self.raw)
    }

    /// Language classification for the symbol.
    pub fn language(&self) -> SymbolLanguage
    {
        self.language
    }
}

impl fmt::Display for SymbolName
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        write!(f, "{}", self.display_name())
    }
}

/// Source code location for a symbol or frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation
{
    /// Absolute or workspace-relative path.
    pub file: String,
    /// Line number, if known.
    pub line: Option<u32>,
    /// Column number, if known.
    pub column: Option<u32>,
}

impl SourceLocation
{
    /// Helper to build a location when only a file is known.
    pub fn from_file(file: impl Into<String>) -> Self
    {
        Self {
            file: file.into(),
            line: None,
            column: None,
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn every_constructor_states_its_source()
    {
        let raw = || "_foo".to_owned();
        assert_eq!(
            SymbolName::from_debug_info(raw(), None, SymbolLanguage::C).source(),
            SymbolSource::DebugInfo
        );
        assert_eq!(
            SymbolName::from_symbol_table(raw(), None, SymbolLanguage::C).source(),
            SymbolSource::SymbolTable
        );
        for source in [SymbolSource::DebugInfo, SymbolSource::SymbolTable] {
            assert_eq!(SymbolName::new(raw(), None, SymbolLanguage::C, source).source(), source);
        }
    }
}
