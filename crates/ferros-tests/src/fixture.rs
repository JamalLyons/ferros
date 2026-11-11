//! Fixture representations, locating logic, and source marker resolution.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

/// Error returned when locating a marker comment in fixture source code fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerError
{
    /// The specified marker was not found in the source text.
    NotFound,
    /// The specified marker appeared more than once in the source text.
    Duplicate,
}

impl Display for MarkerError
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result
    {
        match self {
            Self::NotFound => write!(f, "marker not found in source"),
            Self::Duplicate => write!(f, "marker found multiple times in source"),
        }
    }
}

impl Error for MarkerError {}

/// Searches fixture source code for a trailing `// BREAK: <marker>` comment.
///
/// Returns the 1-based line number of the matching line.
///
/// # Errors
///
/// Returns [`MarkerError::NotFound`] if the marker does not appear in `source`,
/// or [`MarkerError::Duplicate`] if it appears more than once.
pub fn find_marker(source: &str, marker: &str) -> Result<u32, MarkerError>
{
    let target = marker.trim();
    if target.is_empty() {
        return Err(MarkerError::NotFound);
    }

    let mut found_line = None;
    for (idx, line) in source.lines().enumerate() {
        let Ok(line_number) = u32::try_from(idx + 1) else {
            return Err(MarkerError::NotFound);
        };

        if let Some((_, after)) = line.split_once("// BREAK:")
            && after.split_whitespace().next() == Some(target)
        {
            if found_line.is_some() {
                return Err(MarkerError::Duplicate);
            }
            found_line = Some(line_number);
        }
    }

    found_line.ok_or(MarkerError::NotFound)
}

/// A built fixture binary and its associated paths.
#[derive(Debug, Clone)]
pub struct Fixture
{
    name: &'static str,
    path: PathBuf,
}

impl Fixture
{
    /// Creates a new fixture descriptor from its short name and executable path.
    #[must_use]
    pub fn from_built(name: &'static str, path: &'static str) -> Self
    {
        Self {
            name,
            path: PathBuf::from(path),
        }
    }

    /// Returns the short name of the fixture (e.g. `"hello"`).
    #[must_use]
    pub fn name(&self) -> &str
    {
        self.name
    }

    /// Returns the path to the built fixture executable.
    #[must_use]
    pub fn path(&self) -> &Path
    {
        &self.path
    }

    /// Returns the path to the fixture's Rust source file.
    #[must_use]
    pub fn source_path(&self) -> PathBuf
    {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(format!("{}.rs", self.name))
    }

    /// Returns the path to the fixture's `.dSYM` bundle on macOS.
    #[must_use]
    pub fn dsym_path(&self) -> PathBuf
    {
        PathBuf::from(format!("{}.dSYM", self.path.display()))
    }

    /// Resolves a marker name to its 1-based line number in the fixture source.
    ///
    /// # Panics
    ///
    /// Panics if the source file cannot be read, or if the marker is missing
    /// or appears more than once.
    #[must_use]
    pub fn marker_line(&self, marker: &str) -> u32
    {
        let source_file = self.source_path();
        let source = fs::read_to_string(&source_file).unwrap_or_else(|err| {
            panic!(
                "failed to read fixture source for '{}' at {}: {err}",
                self.name,
                source_file.display()
            )
        });

        match find_marker(&source, marker) {
            Ok(line) => line,
            Err(MarkerError::NotFound) => {
                panic!(
                    "marker '{marker}' not found in fixture '{}' at {}",
                    self.name,
                    source_file.display()
                );
            }
            Err(MarkerError::Duplicate) => {
                panic!(
                    "marker '{marker}' appeared more than once in fixture '{}' at {}",
                    self.name,
                    source_file.display()
                );
            }
        }
    }
}

/// Locates a built fixture binary by short name using compile-time cargo environment variables.
#[macro_export]
macro_rules! fixture {
    ($name:literal) => {
        $crate::Fixture::from_built($name, env!(concat!("CARGO_BIN_EXE_ferros-fixture-", $name)))
    };
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn test_find_marker_found()
    {
        let src = "fn main() {\n    let x = 1; // BREAK: hello_print\n}";
        assert_eq!(find_marker(src, "hello_print"), Ok(2));
    }

    #[test]
    fn test_find_marker_not_found()
    {
        let src = "fn main() {\n    let x = 1; // BREAK: hello_print\n}";
        assert_eq!(find_marker(src, "missing"), Err(MarkerError::NotFound));
    }

    #[test]
    fn test_find_marker_duplicate()
    {
        let src = "fn main() {\n    let x = 1; // BREAK: dup\n    let y = 2; // BREAK: dup\n}";
        assert_eq!(find_marker(src, "dup"), Err(MarkerError::Duplicate));
    }

    #[test]
    fn test_find_marker_exact_name_matching()
    {
        let src = "fn main() {\n    let a = 1; // BREAK: in_ab\n    let b = 2; // BREAK: in_a\n}";
        assert_eq!(find_marker(src, "in_a"), Ok(3));
        assert_eq!(find_marker(src, "in_ab"), Ok(2));
        assert_eq!(find_marker(src, "in"), Err(MarkerError::NotFound));
    }

    #[test]
    fn test_find_marker_one_based_numbering()
    {
        let src = "// BREAK: top_line\nfn main() {}\n// BREAK: third_line";
        assert_eq!(find_marker(src, "top_line"), Ok(1));
        assert_eq!(find_marker(src, "third_line"), Ok(3));
    }
}
