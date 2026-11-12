//! The platform boundary: `ferros-core` must not depend on, or contain, OS-specific code.
//!
//! Unlike most files here, these tests only read source files: they launch no process.
//! They therefore need no `harness::run`, no fixtures and no debugger entitlement, and
//! they are not limited to macOS (the boundary has to hold on every OS).

use std::fs;
use std::path::{Path, PathBuf};

/// Crates that would give `ferros-core` access to an OS API (or to the macOS crate).
const OS_DEPENDENCIES: &[&str] = &["mach2", "libproc", "libc", "ferros-macos"];

/// Source text that marks OS-specific code. Checked on every line, comments included,
/// because the rule is that `grep -rn "target_os" crates/ferros-core/src` prints nothing.
const OS_SWITCH: &str = "target_os";

/// Source patterns that mark `unsafe` code. Checked on code lines only, so documentation
/// may mention them. The compiler also forbids `unsafe` in ferros-core
/// (`unsafe_code = "forbid"`); this check keeps that lint from being dropped unnoticed.
const UNSAFE_PATTERNS: &[&str] = &["unsafe {", "unsafe fn", "unsafe impl", "unsafe extern"];

fn core_dir() -> PathBuf
{
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ferros-core")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>)
{
    let entries = fs::read_dir(dir).unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Words of a manifest line: runs of the characters a crate name may contain.
///
/// Looking at every word (instead of parsing `name = ...`) also catches the other ways a
/// manifest can name a dependency: `[dependencies.libc]`, `libc.workspace = true` and
/// `sys = { package = "libc" }`.
fn words(line: &str) -> impl Iterator<Item = &str>
{
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .filter(|word| !word.is_empty())
}

/// The OS dependency a manifest line names, if any. Comment lines name nothing.
fn os_dependency_named(line: &str) -> Option<&'static str>
{
    if line.trim_start().starts_with('#') {
        return None;
    }
    OS_DEPENDENCIES
        .iter()
        .copied()
        .find(|dependency| words(line).any(|word| word == *dependency))
}

/// The forbidden pattern a source line contains, if any.
fn forbidden_pattern(line: &str) -> Option<&'static str>
{
    if line.contains(OS_SWITCH) {
        return Some(OS_SWITCH);
    }
    if line.trim_start().starts_with("//") {
        return None;
    }
    UNSAFE_PATTERNS.iter().copied().find(|pattern| line.contains(pattern))
}

/// Manifest lines that name an OS dependency, in each syntax Cargo accepts.
const MANIFEST_LINES_FORBIDDEN: &[&str] = &[
    "libc = \"0.2\"",
    "mach2 = { version = \"0.6\" }",
    "[dependencies.libc]",
    "[target.'cfg(unix)'.dependencies.libproc]",
    "libc.workspace = true",
    "sys = { package = \"libc\", version = \"0.2\" }",
    "ferros-macos = { path = \"../ferros-macos\" }",
];
const MANIFEST_LINES_ALLOWED: &[&str] = &["# libc is not allowed here", "object = \"0.38\"", "libcore-like = \"1\""];
const SOURCE_LINES_FORBIDDEN: &[&str] = &[
    "#[cfg(target_os = \"macos\")]",
    "// see cfg(target_os)",
    "    unsafe { run() }",
    "unsafe fn f() {}",
];
const SOURCE_LINES_ALLOWED: &[&str] = &["/// Never write unsafe { ... } here", "#![forbid(unsafe_code)]", "let x = 1;"];

#[test]
fn checks_recognise_what_they_forbid()
{
    for line in MANIFEST_LINES_FORBIDDEN {
        assert!(os_dependency_named(line).is_some(), "not recognised: {line}");
    }
    for line in MANIFEST_LINES_ALLOWED {
        assert_eq!(os_dependency_named(line), None, "wrongly flagged: {line}");
    }
    for line in SOURCE_LINES_FORBIDDEN {
        assert!(forbidden_pattern(line).is_some(), "not recognised: {line}");
    }
    for line in SOURCE_LINES_ALLOWED {
        assert_eq!(forbidden_pattern(line), None, "wrongly flagged: {line}");
    }
}

#[test]
fn core_manifest_has_no_os_dependencies()
{
    let manifest = core_dir().join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap_or_else(|err| panic!("read {}: {err}", manifest.display()));
    let mut offending = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if let Some(dependency) = os_dependency_named(line) {
            let location = format!("{}:{}", manifest.display(), index + 1);
            offending.push(format!("{location}: `{dependency}`: {}", line.trim()));
        }
    }
    assert!(
        offending.is_empty(),
        "ferros-core must not depend on OS crates:\n{}",
        offending.join("\n")
    );
}

#[test]
fn core_sources_have_no_target_os_or_unsafe()
{
    let src = core_dir().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files found under {}", src.display());

    let mut offending = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        for (index, line) in text.lines().enumerate() {
            if let Some(pattern) = forbidden_pattern(line) {
                let location = format!("{}:{}", file.display(), index + 1);
                offending.push(format!("{location}: `{pattern}`: {}", line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "ferros-core must hold no OS-specific code and no unsafe:\n{}",
        offending.join("\n")
    );
}
