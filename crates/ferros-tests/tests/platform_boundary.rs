//! Platform ownership across workspace dependencies and source files (ADR 0001).
//!
//! These checks read Cargo metadata and source files without launching any debuggees.
//! The same tests run on Linux and macOS, including through `just check-boundary`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// Third-party crates that would give `ferros-core` access to an OS API.
const OS_DEPENDENCIES: &[&str] = &["mach2", "libproc", "libc"];

/// Source text that marks OS-specific code. Checked on every line, comments included,
/// because the rule is that `grep -rn "target_os" crates/ferros-core/src` prints nothing.
const OS_SWITCH: &str = "target_os";

/// Text that marks Mach-O knowledge, which belongs in `ferros-macos` only (ADR 0001).
/// Matched case-insensitively anywhere on a line, comments included.
const MACH_O_TEXT: &[&str] = &[
    "mach-o",
    "macho",
    "mach_o",
    "machheader",
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
const MACH_O_WORD_STARTS: &[&str] = &["LC_", "lc_", "Lc_", "lC_", "nlist", "Nlist", "NLIST"];

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
const FORMAT_DEPENDENCIES: &[&str] = &["object", "goblin", "macho", "mach_object", "elf", "pelite", "coff"];

// Use parser paths and identifiers so prose about ELF or PE build IDs stays legal.
const ELF_MARKERS: &[&str] = &[
    "elf::",
    "object::elf",
    "object::read::elf",
    "ElfFile",
    "Elf32",
    "Elf64",
    "ELFMAG",
    "ELFCLASS",
    "PT_LOAD",
    "SHT_",
    "NT_GNU_BUILD_ID",
];
const PE_MARKERS: &[&str] = &[
    "pe::",
    "coff::",
    "object::pe",
    "object::coff",
    "object::read::pe",
    "object::read::coff",
    "PeFile",
    "Pe32",
    "Pe64",
    "CoffFile",
    "ImageDosHeader",
    "ImageNtHeaders",
    "IMAGE_DOS_SIGNATURE",
    "IMAGE_NT_SIGNATURE",
    "IMAGE_FILE_MACHINE_",
    "IMAGE_SCN_",
];

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

/// Runs of the characters a Cargo dependency key may contain.
fn words(line: &str) -> impl Iterator<Item = &str>
{
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .filter(|word| !word.is_empty())
}

fn unsafe_on_code_line(line: &str) -> bool
{
    const KEYWORDS: &[&str] = &["fn", "impl", "extern", "trait"];
    !line.trim_start().starts_with("//")
        && line.match_indices("unsafe").any(|(offset, token)| {
            let left = line[..offset].chars().next_back();
            let right = &line[offset + token.len()..];
            let next = right.trim_start();
            !left.is_some_and(identifier_char)
                && (next.is_empty()
                    || next.starts_with('{')
                    || (next.len() != right.len() && KEYWORDS.iter().any(|keyword| next.starts_with(keyword))))
        })
}

fn identifier_char(c: char) -> bool
{
    c.is_alphanumeric() || c == '_'
}

/// Whether `token` starts a word in `line` and (for `LC_`) is followed by a letter.
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

/// The Mach-O marker of `line` from the substring lists only, without the word rules
/// (`LC_`, `nlist`) that also match ordinary words on other platforms.
fn mach_o_parser_marker(line: &str) -> Option<&'static str>
{
    let lower = line.to_ascii_lowercase();
    MACH_O_TEXT
        .iter()
        .copied()
        .find(|token| lower.contains(token))
        .or_else(|| MACH_O_CASED.iter().copied().find(|token| line.contains(token)))
}

/// The macOS runtime marker a source line contains, if any (comments included).
fn macos_runtime_marker(line: &str) -> Option<&'static str>
{
    let lower = line.to_ascii_lowercase();
    MACOS_RUNTIME_TEXT.iter().copied().find(|token| lower.contains(token))
}

fn parser_marker(line: &str, markers: &[&'static str]) -> Option<&'static str>
{
    markers.iter().copied().find(|marker| {
        line.match_indices(marker)
            .any(|(at, _)| !line[..at].chars().next_back().is_some_and(identifier_char))
    })
}

/// The one CLI file, relative to the crate directory, that may switch on the OS and name
/// a platform crate.
const CLI_PLATFORM_FILE: &str = "src/platform.rs";

/// Platform backend crates, today's and the planned ones. A new platform crate is added
/// here; a new portable `ferros-*` crate is not a platform crate.
const PLATFORM_CRATES: &[&str] = &["ferros-macos", "ferros-linux", "ferros-windows"];

fn is_platform_crate(name: &str) -> bool
{
    PLATFORM_CRATES.contains(&name)
}

fn packages(metadata: &Value) -> &[Value]
{
    metadata["packages"].as_array().expect("metadata packages")
}

fn dependencies(package: &Value) -> &[Value]
{
    package["dependencies"].as_array().expect("package dependencies")
}

fn package_name(package: &Value) -> &str
{
    package["name"].as_str().expect("package name")
}

#[derive(Debug)]
struct DependencyViolation<'a>
{
    owner: &'a Value,
    dependency: &'a Value,
    rule: &'static str,
}

fn check_platform_edges(metadata: &Value) -> Vec<DependencyViolation<'_>>
{
    let mut violations = Vec::new();
    for owner in packages(metadata) {
        let owner_name = package_name(owner);
        for dependency in dependencies(owner) {
            // Cargo records the package name separately from its manifest alias. Every
            // declared edge counts, including optional, target-specific, build and dev edges.
            let name = package_name(dependency);
            let platform = is_platform_crate(name);
            let rule = match owner_name {
                "ferros-core" if platform => "ferros-core must not depend on a platform crate",
                "ferros-core" if OS_DEPENDENCIES.contains(&name) => "ferros-core must not depend on OS crates",
                "ferros-core" if FORMAT_DEPENDENCIES.contains(&name) => {
                    "ferros-core must not depend on an object-file parser"
                }
                "ferros" if platform && dependency["rename"].is_string() => "the CLI must not rename a platform dependency",
                _ if platform && owner_name != name && is_platform_crate(owner_name) => {
                    "a platform crate must not depend on another platform crate"
                }
                _ => continue,
            };
            violations.push(DependencyViolation { owner, dependency, rule });
        }
    }
    violations
}

/// Locate the dependency key for diagnostics. Cargo metadata decides whether the edge exists.
fn dependency_location<'a>(manifest: &'a str, key: &str) -> Option<(usize, &'a str)>
{
    manifest.lines().enumerate().find_map(|(index, line)| {
        let code = line.trim_start();
        let matches_key = if code.starts_with('[') {
            let mut keys = words(code.split('#').next().unwrap_or_default());
            keys.any(|word| matches!(word, "dependencies" | "build-dependencies" | "dev-dependencies"))
                && keys.last() == Some(key)
        } else {
            !code.starts_with('#') && code.split_once('=').is_some_and(|(left, _)| words(left).next() == Some(key))
        };
        matches_key.then_some((index + 1, line))
    })
}

fn manifest_path(package: &Value) -> &Path
{
    Path::new(package["manifest_path"].as_str().expect("package manifest path"))
}

fn dependency_diagnostic(root: &Path, violation: &DependencyViolation<'_>) -> String
{
    let path = manifest_path(violation.owner);
    let manifest = read_text(path);
    let name = package_name(violation.dependency);
    let key = violation.dependency["rename"].as_str().unwrap_or(name);
    let (number, line) = dependency_location(&manifest, key).unwrap_or((1, key));
    let file = path.strip_prefix(root).unwrap_or(path).display();
    let mut diagnostic = format!("{file}:{number}: {}: ", violation.rule);
    write!(diagnostic, "{line} (package `{name}`)").expect("write diagnostic");
    diagnostic
}

/// The CLI's own rule, enforced on its own: only `src/platform.rs` may hold a
/// `cfg(target_os)` switch or name a platform crate. `path` is relative to the crate
/// directory, so `build.rs`, `examples/`, `tests/` and other `src/` files are all checked.
fn cli_violation<'a>(path: &Path, line: &str, cli_platform_names: &'a BTreeSet<String>) -> Option<(&'static str, &'a str)>
{
    if path == Path::new(CLI_PLATFORM_FILE) {
        return None;
    }
    if line.contains(OS_SWITCH) {
        return Some(("only platform.rs may contain target_os in the CLI", OS_SWITCH));
    }
    cli_platform_names
        .iter()
        .find(|name| line.contains(name.as_str()))
        .map(|name| ("only platform.rs may name a platform crate in the CLI", name.as_str()))
}

/// Checks `path` (relative to the crate directory) against the rules for crate `owner`.
fn source_violation<'a>(
    owner: &str,
    path: &Path,
    line: &str,
    cli_platform_names: &'a BTreeSet<String>,
) -> Option<(&'static str, &'a str)>
{
    match owner {
        "ferros" => return cli_violation(path, line, cli_platform_names),
        "ferros-core" => {
            if line.contains(OS_SWITCH) {
                return Some(("ferros-core must not contain target_os", OS_SWITCH));
            }
            if unsafe_on_code_line(line) {
                return Some(("ferros-core must not contain unsafe code", "unsafe"));
            }
        }
        _ => {}
    }
    if owner == "ferros-core" {
        if let Some(marker) = mach_o_marker(line) {
            return Some(("Mach-O knowledge belongs in ferros-macos", marker));
        }
        if let Some(marker) = macos_runtime_marker(line) {
            return Some(("macOS runtime knowledge belongs in ferros-macos", marker));
        }
    } else if owner != "ferros-macos"
        && let Some(marker) = mach_o_parser_marker(line)
    {
        // Another platform crate may name its own signal frame, locale variables and
        // libc symbols, which share words with the macOS runtime and `LC_` rules; only
        // Mach-O parsing is reserved to ferros-macos there.
        return Some(("Mach-O parsing belongs in ferros-macos", marker));
    }
    if owner != "ferros-linux"
        && let Some(marker) = parser_marker(line, ELF_MARKERS)
    {
        return Some(("ELF parsing belongs in ferros-linux", marker));
    }
    if owner != "ferros-windows"
        && let Some(marker) = parser_marker(line, PE_MARKERS)
    {
        return Some(("PE/COFF parsing belongs in ferros-windows", marker));
    }
    None
}

fn read_text(path: &Path) -> String
{
    fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// A place Cargo compiles from: a directory of Rust files, or one file that sits directly
/// in the crate directory (`build.rs`, a root-level `main.rs`).
#[derive(Debug, PartialEq, Eq)]
enum TargetRoot
{
    Dir(PathBuf),
    File(PathBuf),
}

/// Every target root of `package`, from Cargo metadata: library, binaries, examples,
/// tests, benches and the custom build script. A `build.rs` is its own root, so the whole
/// crate directory (with `target/`) is never walked.
fn target_roots(package: &Value) -> Vec<TargetRoot>
{
    let package_dir = manifest_path(package).parent().expect("manifest directory");
    let mut roots = BTreeSet::new();
    for target in package["targets"].as_array().expect("package targets") {
        let src_path = Path::new(target["src_path"].as_str().expect("target src_path"));
        let parent = src_path.parent().expect("target source directory");
        roots.insert(if parent == package_dir {
            TargetRoot::File(src_path.to_path_buf())
        } else {
            TargetRoot::Dir(parent.to_path_buf())
        });
    }
    roots.into_iter().collect()
}

impl Ord for TargetRoot
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering
    {
        self.path().cmp(other.path())
    }
}

impl PartialOrd for TargetRoot
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering>
    {
        Some(self.cmp(other))
    }
}

impl TargetRoot
{
    fn path(&self) -> &Path
    {
        match self {
            Self::Dir(path) | Self::File(path) => path,
        }
    }
}

/// The sorted, deduplicated Rust files under `roots`.
fn target_files(roots: &[TargetRoot]) -> Vec<PathBuf>
{
    let mut files = Vec::new();
    for root in roots {
        match root {
            TargetRoot::Dir(dir) => rust_files(dir, &mut files),
            TargetRoot::File(file) => files.push(file.clone()),
        }
    }
    files.sort_unstable();
    files.dedup();
    files
}

/// Checks the source files of one crate. `files` pairs each file's path with its text;
/// nothing here touches the file system, so tests feed it synthetic input. Paths must lie
/// under `package_dir`; diagnostics print them relative to `root`.
fn check_sources(
    root: &Path,
    owner: &str,
    package_dir: &Path,
    files: &[(PathBuf, String)],
    cli_platform_names: &BTreeSet<String>,
) -> Vec<String>
{
    let mut offending = Vec::new();
    for (path, text) in files {
        let file = path.strip_prefix(root).unwrap_or(path).display();
        let relative = path.strip_prefix(package_dir).expect("source path relative to its crate");
        if owner == "ferros-core"
            && relative == Path::new("src/lib.rs")
            && !text.lines().any(|line| line.starts_with("#![forbid(unsafe_code)]"))
        {
            offending.push(format!(
                "{file}:1: ferros-core must forbid unsafe code: missing #![forbid(unsafe_code)]"
            ));
        }
        for (index, line) in text.lines().enumerate() {
            if let Some((rule, marker)) = source_violation(owner, relative, line, cli_platform_names) {
                let mut diagnostic = format!("{file}:{}: ", index + 1);
                write!(diagnostic, "{rule} ({marker}): {line}").expect("write diagnostic");
                offending.push(diagnostic);
            }
        }
    }
    offending
}

fn platform_crate_names(metadata: &Value) -> BTreeSet<String>
{
    packages(metadata)
        .iter()
        .flat_map(|package| std::iter::once(package).chain(dependencies(package)))
        .map(package_name)
        .filter(|name| is_platform_crate(name))
        .map(|name| name.replace('-', "_"))
        .collect()
}

fn check_workspace(root: &Path, metadata: &Value) -> Vec<String>
{
    let mut offending = check_platform_edges(metadata)
        .iter()
        .map(|violation| dependency_diagnostic(root, violation))
        .collect::<Vec<_>>();
    let cli_platform_names = platform_crate_names(metadata);

    for package in packages(metadata) {
        let owner = package_name(package);
        if !matches!(owner, "ferros-core" | "ferros") && !is_platform_crate(owner) {
            continue;
        }
        let package_dir = manifest_path(package).parent().expect("manifest directory");
        let paths = target_files(&target_roots(package));
        assert!(!paths.is_empty(), "no .rs files found for {owner}");
        let files = paths
            .into_iter()
            .map(|path| {
                let text = read_text(&path);
                (path, text)
            })
            .collect::<Vec<_>>();
        offending.extend(check_sources(root, owner, package_dir, &files, &cli_platform_names));
    }
    offending
}

#[test]
fn workspace_boundary()
{
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent().and_then(Path::parent).expect("workspace root");
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into()))
        .arg("metadata")
        .args(["--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata = serde_json::from_slice(&output.stdout).expect("parse Cargo metadata");
    let offending = check_workspace(root, &metadata);
    assert!(offending.is_empty(), "\n{}", offending.join("\n"));
}

const SOURCE_LINES_FORBIDDEN: &[&str] = &[
    "#[cfg(target_os = \"macos\")]",
    "// see cfg(target_os)",
    "    unsafe { run() }",
    "unsafe fn f() {}",
    "unsafe{ run() }",
    "unsafe\timpl Trait for T {}",
    "unsafe",
];
const SOURCE_LINES_ALLOWED: &[&str] = &["/// Never write unsafe { ... } here", "#![forbid(unsafe_code)]", "let x = 1;"];
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
fn renamed_optional_platform_dependency_is_rejected()
{
    const EXPECTED: &[(&str, &str)] = &[("ferros-core", "ferros-linux"), ("ferros-linux", "ferros-macos")];
    let metadata = json!({
        "version": 1,
        "packages": [
            {
                "name": "ferros-core",
                "dependencies": [{
                    "name": "ferros-linux",
                    "rename": "os",
                    "optional": true,
                    "target": "cfg(target_os = \"linux\")",
                    "kind": "normal",
                }],
            },
            {
                "name": "ferros-linux",
                "dependencies": [{
                    "name": "ferros-macos",
                    "rename": null,
                    "optional": false,
                    "target": null,
                    "kind": "normal",
                }],
            },
            {"name": "ferros-macos", "dependencies": []},
        ],
    });
    let violations = check_platform_edges(&metadata);
    let edges = violations
        .iter()
        .map(|violation| (package_name(violation.owner), package_name(violation.dependency)))
        .collect::<Vec<_>>();
    assert_eq!(edges, EXPECTED);
}

#[test]
fn allowed_platform_and_portable_dependencies_pass()
{
    let metadata = json!({
        "version": 1,
        "packages": [
            {"name": "ferros", "dependencies": [
                {"name": "ferros-macos", "target": "cfg(target_os = \"macos\")"},
                {"name": "ferros-windows", "target": "cfg(target_os = \"windows\")"},
            ]},
            {"name": "ferros-tests", "dependencies": [
                {"name": "ferros-macos", "kind": "dev", "target": "cfg(target_os = \"macos\")"},
            ]},
            {"name": "ferros-core", "dependencies": [
                {"name": "gimli"}, {"name": "libcore-like"}, {"name": "objects"}, {"name": "ferros-utils"},
            ]},
            {"name": "ferros-macos", "dependencies": [
                {"name": "ferros-core"}, {"name": "mach2"}, {"name": "object"},
            ]},
        ],
    });
    let violations = check_platform_edges(&metadata);
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn os_and_format_dependencies_and_cli_renames_are_rejected()
{
    const EXPECTED: &[&str] = &["libc", "mach2", "libproc", "object", "ferros-windows", "ferros-macos"];
    let metadata = json!({
        "version": 1,
        "packages": [
            {"name": "ferros-core", "dependencies": [
                {"name": "libc", "rename": "sys"},
                {"name": "mach2"},
                {"name": "libproc", "target": "cfg(unix)"},
                {"name": "object", "kind": "dev"},
                {"name": "ferros-windows", "kind": "build"},
            ]},
            {"name": "ferros", "dependencies": [
                {"name": "ferros-macos", "rename": "adapter", "target": "cfg(target_os = \"macos\")"},
            ]},
        ],
    });
    let violations = check_platform_edges(&metadata);
    let names = violations
        .iter()
        .map(|violation| package_name(violation.dependency))
        .collect::<Vec<_>>();
    assert_eq!(names, EXPECTED);
}

#[test]
fn dependency_locations_use_cargo_keys()
{
    const DECLARATIONS: &[(&str, &str)] = &[
        ("libc", "libc = \"0.2\""),
        ("mach2", "mach2 = { version = \"0.6\" }"),
        ("libc", "[dependencies.libc]"),
        ("libproc", "[target.'cfg(unix)'.dependencies.libproc]"),
        ("libc", "libc.workspace = true"),
        ("sys", "sys = { package = \"libc\", version = \"0.2\" }"),
        ("ferros-macos", "ferros-macos = { path = \"../ferros-macos\" }"),
        ("object", "object = { version = \"0.41\", features = [\"read\"] }"),
        ("object", "object.workspace = true"),
        ("object", "[dependencies.object]"),
        ("os", "[target.'cfg(target_os = \"linux\")'.dependencies.os]"),
    ];
    for &(key, declaration) in DECLARATIONS {
        let manifest = format!("# {declaration}\n{declaration}");
        assert_eq!(dependency_location(&manifest, key).map(|(_, line)| line), Some(declaration));
    }
}

#[test]
fn ferros_utils_forbids_unsafe_code()
{
    let manifest = include_str!("../../ferros-utils/Cargo.toml");
    let rust_lints = manifest
        .split_once("[lints.rust]")
        .map(|(_, lints)| lints.lines().take_while(|line| !line.trim_start().starts_with('[')));
    assert!(
        rust_lints.is_some_and(|mut lints| lints.any(|line| line.trim() == "unsafe_code = \"forbid\"")),
        "ferros-utils must forbid unsafe code in its rustc lints"
    );
}

#[test]
fn checks_recognise_what_they_forbid()
{
    let cli_platform_names = BTreeSet::new();
    let path = Path::new("lib.rs");
    for line in SOURCE_LINES_FORBIDDEN
        .iter()
        .chain(MACH_O_LINES_FORBIDDEN)
        .chain(MACOS_RUNTIME_LINES_FORBIDDEN)
    {
        assert!(
            source_violation("ferros-core", path, line, &cli_platform_names).is_some(),
            "not recognised: {line}"
        );
    }
    for line in SOURCE_LINES_ALLOWED
        .iter()
        .chain(MACH_O_LINES_ALLOWED)
        .chain(MACOS_RUNTIME_LINES_ALLOWED)
    {
        assert_eq!(
            source_violation("ferros-core", path, line, &cli_platform_names),
            None,
            "wrongly flagged: {line}"
        );
    }
}

#[test]
fn object_format_parsing_stays_with_its_platform()
{
    const OWNERS: &[&str] = &["ferros-core", "ferros-macos", "ferros-linux", "ferros-windows"];
    const FORMAT_FIXTURES: &[(&str, [&str; 2])] = &[
        (
            "ferros-macos",
            ["use object::macho::MachHeader64;", "fn read(header: &MachHeader64) {}"],
        ),
        (
            "ferros-linux",
            ["use object::read::elf::ElfFile64;", "fn read(header: &Elf64_Ehdr) {}"],
        ),
        (
            "ferros-windows",
            ["use object::read::pe::PeFile64;", "fn read(header: &ImageNtHeaders64) {}"],
        ),
    ];
    let cli_platform_names = BTreeSet::new();
    let path = Path::new("image.rs");
    for &(format_owner, lines) in FORMAT_FIXTURES {
        for &owner in OWNERS {
            for line in lines {
                assert_eq!(
                    source_violation(owner, path, line, &cli_platform_names).is_some(),
                    owner != format_owner,
                    "{owner}: {line}"
                );
            }
        }
    }
    for &owner in OWNERS {
        for line in [
            "/// Accepts ELF or PE build IDs.",
            "use gimli::EhFrame;",
            "let entry = Scope::new();",
        ] {
            assert_eq!(
                source_violation(owner, path, line, &cli_platform_names),
                None,
                "{owner}: {line}"
            );
        }
    }
}

#[test]
fn other_platforms_may_use_words_the_macos_rules_reserve_in_core()
{
    const LINES: &[&str] = &[
        "let regs = &frame.uc_mcontext;",
        "struct rt_sigframe;",
        "cmd.env(\"LC_ALL\", \"C\");",
        "extern \"C\" { fn nlist(); }",
    ];
    let cli_platform_names = BTreeSet::new();
    let path = Path::new("signal.rs");
    for line in LINES {
        assert_eq!(
            source_violation("ferros-linux", path, line, &cli_platform_names),
            None,
            "ferros-linux: {line}"
        );
        assert!(
            source_violation("ferros-core", path, line, &cli_platform_names).is_some(),
            "ferros-core: {line}"
        );
    }
}

#[test]
fn portable_crates_are_not_platform_crates()
{
    let metadata = serde_json::json!({
        "packages": [{
            "name": "ferros-core",
            "manifest_path": "crates/ferros-core/Cargo.toml",
            "dependencies": [{ "name": "ferros-dwarf", "rename": null, "optional": false, "target": null, "kind": null }]
        }]
    });
    assert!(check_platform_edges(&metadata).is_empty());
    let metadata = serde_json::json!({
        "packages": [{
            "name": "ferros-core",
            "manifest_path": "crates/ferros-core/Cargo.toml",
            "dependencies": [{ "name": "ferros-linux", "rename": null, "optional": false, "target": null, "kind": null }]
        }]
    });
    let violations = check_platform_edges(&metadata);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].rule, "ferros-core must not depend on a platform crate");
}

#[test]
fn cli_platform_names_stay_in_the_factory()
{
    let cli_platform_names = BTreeSet::from(["ferros_macos".to_owned(), "ferros_linux".to_owned()]);
    for line in ["use ferros_macos::new_session;", "use ferros_linux::new_session;"] {
        assert!(source_violation("ferros", Path::new("src/main.rs"), line, &cli_platform_names).is_some());
        assert_eq!(
            source_violation("ferros", Path::new("src/platform.rs"), line, &cli_platform_names),
            None
        );
    }
}

/// A synthetic crate: its directory and `(file, text)` pairs, as `check_sources` takes them.
fn synthetic_files(package_dir: &str, files: &[(&str, &str)]) -> (PathBuf, Vec<(PathBuf, String)>)
{
    let dir = PathBuf::from(package_dir);
    let files = files.iter().map(|&(name, text)| (dir.join(name), text.to_owned())).collect();
    (dir, files)
}

fn synthetic_report(owner: &str, package_dir: &str, files: &[(&str, &str)]) -> Vec<String>
{
    let (dir, files) = synthetic_files(package_dir, files);
    let names = BTreeSet::from(["ferros_macos".to_owned()]);
    check_sources(Path::new(""), owner, &dir, &files, &names)
}

#[test]
fn cli_target_os_outside_platform_rs_is_rejected()
{
    const SWITCH: &str = "#[cfg(target_os = \"macos\")]\nfn only_here() {}\n";
    const FILES: &[(&str, &str)] = &[
        ("src/platform.rs", SWITCH),
        ("src/main.rs", "fn main() {}\n"),
        ("src/repl.rs", SWITCH),
    ];
    let report = synthetic_report("ferros", "crates/ferros", FILES);
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(
        report[0].starts_with("crates/ferros/src/repl.rs:1: only platform.rs may contain target_os"),
        "{report:?}"
    );
}

#[test]
fn cli_platform_crate_names_outside_platform_rs_are_rejected()
{
    const FILES: &[(&str, &str)] = &[
        ("src/platform.rs", "use ferros_macos::new_session;\n"),
        ("src/commands/launch.rs", "use ferros_macos::MacOSBackend;\n"),
    ];
    let report = synthetic_report("ferros", "crates/ferros", FILES);
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(
        report[0].starts_with("crates/ferros/src/commands/launch.rs:1: "),
        "{report:?}"
    );
}

#[test]
fn cli_files_besides_platform_rs_are_checked_in_every_target_root()
{
    // Only `src/platform.rs` is exempt: a same-named file elsewhere is not.
    const FILES: &[(&str, &str)] = &[
        ("src/bin/platform.rs", "#[cfg(target_os = \"linux\")]\n"),
        ("examples/platform.rs", "#[cfg(target_os = \"linux\")]\n"),
        ("tests/probe.rs", "use ferros_macos::new_session;\n"),
        ("build.rs", "fn main() { if cfg!(target_os = \"macos\") {} }\n"),
    ];
    let report = synthetic_report("ferros", "crates/ferros", FILES);
    assert_eq!(report.len(), 4, "{report:?}");
}

#[test]
fn build_script_violations_are_reported()
{
    const CORE: &[(&str, &str)] = &[
        ("src/lib.rs", "#![forbid(unsafe_code)]\n"),
        ("build.rs", "fn main() { if cfg!(target_os = \"macos\") {} }\n"),
    ];
    let core = synthetic_report("ferros-core", "crates/ferros-core", CORE);
    assert_eq!(core.len(), 1, "{core:?}");
    assert!(
        core[0].starts_with("crates/ferros-core/build.rs:1: ferros-core must not contain target_os"),
        "{core:?}"
    );
    let linux = synthetic_report(
        "ferros-linux",
        "crates/ferros-linux",
        &[("build.rs", "use object::macho::MachHeader64;\n")],
    );
    assert_eq!(linux.len(), 1, "{linux:?}");
    assert!(linux[0].contains("Mach-O parsing belongs in ferros-macos"), "{linux:?}");
}

#[test]
fn core_example_bench_and_test_roots_are_checked()
{
    const FILES: &[(&str, &str)] = &[
        ("src/lib.rs", "#![forbid(unsafe_code)]\n"),
        ("benches/speed.rs", "let k = LC_SYMTAB;\n"),
        ("examples/demo.rs", "unsafe { run() }\n"),
        ("tests/probe/main.rs", "// via dyld\n"),
    ];
    let report = synthetic_report("ferros-core", "crates/ferros-core", FILES);
    assert_eq!(report.len(), 3, "{report:?}");
}

#[test]
fn core_lib_must_still_forbid_unsafe_code()
{
    let report = synthetic_report("ferros-core", "crates/ferros-core", &[("src/lib.rs", "pub mod a;\n")]);
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(report[0].contains("missing #![forbid(unsafe_code)]"), "{report:?}");
}

#[test]
fn target_roots_cover_every_cargo_target()
{
    let package = json!({
        "name": "ferros",
        "manifest_path": "/w/crates/ferros/Cargo.toml",
        "targets": [
            {"kind": ["bin"], "src_path": "/w/crates/ferros/src/main.rs"},
            {"kind": ["bin"], "src_path": "/w/crates/ferros/src/bin/other.rs"},
            {"kind": ["example"], "src_path": "/w/crates/ferros/examples/demo.rs"},
            {"kind": ["test"], "src_path": "/w/crates/ferros/tests/probe/main.rs"},
            {"kind": ["bench"], "src_path": "/w/crates/ferros/benches/speed.rs"},
            {"kind": ["custom-build"], "src_path": "/w/crates/ferros/build.rs"},
            {"kind": ["custom-build"], "src_path": "/w/crates/ferros/scripts/setup.rs"},
        ],
    });
    let roots = target_roots(&package);
    let expected_roots = vec![
        TargetRoot::File(PathBuf::from("/w/crates/ferros/build.rs")),
        TargetRoot::Dir(PathBuf::from("/w/crates/ferros/src")),
        TargetRoot::Dir(PathBuf::from("/w/crates/ferros/src/bin")),
        TargetRoot::Dir(PathBuf::from("/w/crates/ferros/examples")),
        TargetRoot::Dir(PathBuf::from("/w/crates/ferros/tests/probe")),
        TargetRoot::Dir(PathBuf::from("/w/crates/ferros/benches")),
        TargetRoot::Dir(PathBuf::from("/w/crates/ferros/scripts")),
    ];
    for expected in expected_roots {
        assert!(roots.contains(&expected), "missing {expected:?} in {roots:?}");
    }
    assert_eq!(roots.len(), 7, "{roots:?}");
}
