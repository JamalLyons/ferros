//! Symbol and source location types.

use std::fmt;

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

/// A function or type name with demangling metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolName
{
    raw: String,
    demangled: Option<String>,
    language: SymbolLanguage,
}

impl SymbolName
{
    /// Construct from a raw linkage name.
    pub fn new(raw: String, demangled: Option<String>, language: SymbolLanguage) -> Self
    {
        Self {
            raw,
            demangled,
            language,
        }
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
