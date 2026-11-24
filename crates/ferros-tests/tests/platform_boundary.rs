//! The platform boundary: `ferros-core` must not depend on, or contain, OS-specific or
//! object-file-format code (ADR 0001, #193).
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

/// Text that marks Mach-O knowledge, which belongs in `ferros-macos` only (ADR 0001).
/// Matched case-insensitively anywhere on a line, comments included.
const MACH_O_TEXT: &[&str] = &[
    "mach-o",
    "macho",
    "mach_o",
    "mh_magic",
    "feedfacf",
    "feed_facf",
    "mach_uuid",
    "__linkedit",
    "__pagezero",
    "__unwind_info",
    "compact unwind",
    "compact_unwind",
    "dsymutil",
    "n_oso",
    "mh_execute",
    "mh_dylib",
    "cpu_type_",
    "cpu_subtype_",
    "cafebabe",
    "cafe_babe",
    "feedface",
    "feed_face",
    "cffaedfe",
    "cefaedfe",
    "bebafeca",
    "fat_arch",
    "fat_header",
    "segment_command",
    "dyld_info",
    "uuidcommand",
    "uuid_command",
    "unwind_arm64_mode",
    "s_attr_",
    "\"__text\"",
    "\"__data\"",
    "__text,",
];

/// Mach-O text matched with its case, where a case-insensitive match would flag neutral
/// words (`MappedSymbols` contains `dsym`).
const MACH_O_CASED: &[&str] = &["dSYM"];

/// Mach-O words that are only Mach-O as a whole word start: `LC_*` load commands and the
/// `nlist` symbol entry. A plain substring would flag words such as `unlisted`. An
/// underscore does not end a word, so `struct_nlist` is not caught, but `_nlist` is
/// (see [`starts_word`]).
const MACH_O_WORD_STARTS: &[&str] = &["LC_", "lc_", "nlist", "Nlist", "NLIST"];

/// Text that marks macOS operating-system knowledge: its system libraries and the xnu
/// signal frame. The platform supplies these through `runtime::TargetRuntime` (#201).
/// Matched case-insensitively anywhere on a line, comments included.
const MACOS_RUNTIME_TEXT: &[&str] = &[
    "dylib",
    "dyld",
    "sigtramp",
    "ucontext",
    "mcontext",
    "sigframe",
    "uc_mcsize",
    "xnu",
    "libunwind",
    "libsystem_",
    "unw_resume",
    "__longjmp",
];

/// Crates that parse object-file containers. `ferros-core` takes DWARF and CFI as bytes
/// from its `ObjectFormat`, so it never needs one.
const FORMAT_DEPENDENCIES: &[&str] = &["object", "goblin", "macho", "mach_object"];

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

/// The object-file container crate a manifest line names, if any. Comment lines name nothing.
fn format_dependency_named(line: &str) -> Option<&'static str>
{
    if line.trim_start().starts_with('#') {
        return None;
    }
    FORMAT_DEPENDENCIES
        .iter()
        .copied()
        .find(|dependency| words(line).any(|word| word == *dependency))
}

/// Whether `token` starts a word at some position of `line`, and (for `LC_`) is followed by
/// an upper-case letter as a load-command name is.
fn starts_word(line: &str, token: &str) -> bool
{
    line.match_indices(token).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + token.len()..].chars().next();
        let boundary = !before.is_some_and(|c| c.is_ascii_alphanumeric());
        let named = !token.ends_with('_') || after.is_some_and(|c| c.is_ascii_alphabetic());
        boundary && named
    })
}

/// The Mach-O marker a source line contains, if any. Comments are checked too: the rule
/// is that no Mach-O knowledge, described or implemented, lives in `ferros-core`.
fn mach_o_marker(line: &str) -> Option<&'static str>
{
    let lower = line.to_ascii_lowercase();
    MACH_O_TEXT
        .iter()
        .copied()
        .find(|token| lower.contains(token))
        .or_else(|| MACH_O_CASED.iter().copied().find(|token| line.contains(token)))
        .or_else(|| MACH_O_WORD_STARTS.iter().copied().find(|token| starts_word(line, token)))
}

/// The macOS runtime marker a source line contains, if any (comments included).
fn macos_runtime_marker(line: &str) -> Option<&'static str>
{
    let lower = line.to_ascii_lowercase();
    MACOS_RUNTIME_TEXT.iter().copied().find(|token| lower.contains(token))
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
const FORMAT_MANIFEST_LINES_FORBIDDEN: &[&str] = &[
    "object = { version = \"0.41\", features = [\"read\"] }",
    "object.workspace = true",
    "[dependencies.object]",
];
const FORMAT_MANIFEST_LINES_ALLOWED: &[&str] = &["# no object crate here", "objects = \"1\"", "gimli = \"=0.32.3\""];
const MACH_O_LINES_FORBIDDEN: &[&str] = &[
    "const MH_MAGIC_64: u32 = 0xfeed_facf;",
    "if magic == 0xfeedfacf {",
    "LC_SYMTAB => {}",
    "let cmd = LC_UUID;",
    "use object::macho::MachHeader64;",
    "/// Reads the Mach-O header.",
    "// skip __PAGEZERO",
    "let linkedit = \"__LINKEDIT\";",
    "let info = section(\"__unwind_info\");",
    "mod compact_unwind;",
    "/// uses compact unwind first",
    "let n = nlist.len();",
    "struct Nlist64;",
    "let uuid = mach_uuid(file);",
    "/// the .dSYM bundle",
    "// run dsymutil",
    "N_OSO entries",
    "const MH_EXECUTE: u32 = 2;",
    "let arm = CPU_TYPE_ARM64;",
    "const FAT_MAGIC: u32 = 0xcafebabe;",
    "0xCAFE_BABE",
    "const MH_MAGIC: u32 = 0xfeedface;",
    "b\"\\xcf\\xfa\\xed\\xfe\" // cffaedfe",
    "struct FatArch; // fat_arch",
    "segment_command_64",
    "UuidCommand",
    "UNWIND_ARM64_MODE_FRAME",
    "S_ATTR_PURE_INSTRUCTIONS",
    "section(\"__TEXT\")",
    "let s = \"__TEXT,__text\";",
    "NLIST_64",
    "lc_uuid",
    "LC_segment",
];
const MACOS_RUNTIME_LINES_FORBIDDEN: &[&str] = &[
    "image: \"libunwind.dylib\",",
    "/// in `_sigtramp`",
    "const UCONTEXT: u64 = 104;",
    "let mcontext = frame + 160;",
    "mod sigframe {",
    "// uc_mcsize is 816",
    "\"/usr/lib/libSystem.B.DYLIB\"",
    "/// xnu's sendsig",
    "const RESUME: &str = \"unw_resume\";",
    "image: \"libsystem_platform\",",
    "// libunwind resumes here",
    "if name == \"__longjmp\" {",
    "/// If dyld has not published it",
];
const MACOS_RUNTIME_LINES_ALLOWED: &[&str] = &[
    "/// the signal trampoline",
    "let context = runtime.signal_context(memory, &frame);",
    "/// a dynamic library",
    "\"libexits.so.1\"",
];
const MACH_O_LINES_ALLOWED: &[&str] = &[
    "/// An unlisted image",
    "fn unlisted_image_alignment()",
    "let calc_offset = 1;",
    "/// The DWARF unwind table",
    "use crate::objfile::MappedSymbols;",
    "let text_section = 1;",
    "/// the text segment",
    "fn calc_uuid()",
];

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
    for line in FORMAT_MANIFEST_LINES_FORBIDDEN {
        assert!(format_dependency_named(line).is_some(), "not recognised: {line}");
    }
    for line in FORMAT_MANIFEST_LINES_ALLOWED {
        assert_eq!(format_dependency_named(line), None, "wrongly flagged: {line}");
    }
    for line in MACH_O_LINES_FORBIDDEN {
        assert!(mach_o_marker(line).is_some(), "not recognised: {line}");
    }
    for line in MACH_O_LINES_ALLOWED {
        assert_eq!(mach_o_marker(line), None, "wrongly flagged: {line}");
    }
    for line in MACOS_RUNTIME_LINES_FORBIDDEN {
        assert!(macos_runtime_marker(line).is_some(), "not recognised: {line}");
    }
    for line in MACOS_RUNTIME_LINES_ALLOWED {
        assert_eq!(macos_runtime_marker(line), None, "wrongly flagged: {line}");
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

#[test]
fn core_manifest_has_no_object_file_parser()
{
    let manifest = core_dir().join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap_or_else(|err| panic!("read {}: {err}", manifest.display()));
    let mut offending = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if let Some(dependency) = format_dependency_named(line) {
            let location = format!("{}:{}", manifest.display(), index + 1);
            offending.push(format!("{location}: `{dependency}`: {}", line.trim()));
        }
    }
    assert!(
        offending.is_empty(),
        "ferros-core must not depend on an object-file parser (ADR 0001):\n{}",
        offending.join("\n")
    );
}

#[test]
fn core_sources_have_no_mach_o_knowledge()
{
    let src = core_dir().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files found under {}", src.display());

    let mut offending = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        for (index, line) in text.lines().enumerate() {
            if let Some(marker) = mach_o_marker(line) {
                let location = format!("{}:{}", file.display(), index + 1);
                offending.push(format!("{location}: `{marker}`: {}", line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "Mach-O knowledge belongs in ferros-macos, not ferros-core (ADR 0001):\n{}",
        offending.join("\n")
    );
}

#[test]
fn core_sources_have_no_macos_runtime_knowledge()
{
    let src = core_dir().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files found under {}", src.display());

    let mut offending = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        for (index, line) in text.lines().enumerate() {
            if let Some(marker) = macos_runtime_marker(line) {
                let location = format!("{}:{}", file.display(), index + 1);
                offending.push(format!("{location}: `{marker}`: {}", line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "macOS runtime knowledge (system libraries, the signal frame) belongs in ferros-macos (#201):\n{}",
        offending.join("\n")
    );
}
