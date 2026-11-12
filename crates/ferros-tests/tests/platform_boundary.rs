//! The platform boundary: `ferros-core` must not depend on, or contain, OS-specific code.
//!
//! Unlike most files here, these tests only read source files: they launch no process.
//! They therefore need no `harness::run`, no fixtures and no debugger entitlement, and
//! they are not limited to macOS (the boundary has to hold on every OS).

use std::fs;
use std::path::{Path, PathBuf};

/// Crates that would give `ferros-core` access to an OS API (or to the macOS crate).
const OS_DEPENDENCIES: &[&str] = &["mach2", "libproc", "libc", "ferros-macos"];

/// Source patterns that mark OS-specific code or `unsafe` code.
const FORBIDDEN_PATTERNS: &[&str] = &["target_os", "unsafe {", "unsafe fn", "unsafe impl", "unsafe extern"];

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

/// Dependency name declared by a manifest line such as `libc = "0.2"` or
/// `mach2 = { version = "0.6" }`, if the line declares one.
fn declared_name(line: &str) -> Option<&str>
{
    let line = line.trim();
    if line.starts_with('#') || line.starts_with('[') {
        return None;
    }
    let (name, _) = line.split_once('=')?;
    Some(name.trim().trim_matches('"'))
}

#[test]
fn core_manifest_has_no_os_dependencies()
{
    let manifest = core_dir().join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap_or_else(|err| panic!("read {}: {err}", manifest.display()));
    let mut offending = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if let Some(name) = declared_name(line)
            && OS_DEPENDENCIES.contains(&name)
        {
            offending.push(format!("{}:{}: {}", manifest.display(), index + 1, line.trim()));
        }
        if line.contains("ferros-macos") && !line.trim_start().starts_with('#') {
            offending.push(format!("{}:{}: {}", manifest.display(), index + 1, line.trim()));
        }
    }
    offending.dedup();
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
            if let Some(pattern) = FORBIDDEN_PATTERNS.iter().find(|pattern| line.contains(*pattern)) {
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
