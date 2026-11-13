//! Portable discovery of embedded and companion DWARF.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
use object::{Object, ObjectSection};

use crate::error::{DebuggerError, Result};

/// Locations and limits used when looking for companion debug information.
///
/// Supplying these explicitly keeps callers and tests independent of the user's cache
/// and PATH. `dsymutil: None` disables generation, while still allowing cached DWARF.
#[derive(Clone, Debug)]
pub struct DebugInfoOptions
{
    /// Directory containing UUID- and architecture-keyed DWARF files.
    pub cache_directory: PathBuf,
    /// Tool to invoke when no valid companion is present.
    pub dsymutil: Option<PathBuf>,
    /// Maximum wall time allowed for a single tool invocation.
    pub tool_timeout: Duration,
}

impl Default for DebugInfoOptions
{
    fn default() -> Self
    {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        Self {
            cache_directory: root.join("ferros").join("debuginfo"),
            dsymutil: Some(PathBuf::from("dsymutil")),
            tool_timeout: Duration::from_secs(30),
        }
    }
}

fn has_dwarf(file: &object::File<'_>) -> bool
{
    [
        [".debug_info", "__debug_info"],
        [".debug_abbrev", "__debug_abbrev"],
        [".debug_line", "__debug_line"],
    ]
    .iter()
    .all(|names| {
        names
            .iter()
            .any(|name| file.section_by_name(name).is_some_and(|section| section.size() != 0))
    })
}

fn no_debug_info(path: &Path, detail: impl Into<String>) -> DebuggerError
{
    DebuggerError::NoDebugInfo {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

fn valid_slice(bytes: &[u8], uuid: [u8; 16], architecture: object::Architecture) -> Option<&[u8]>
{
    let file = object::File::parse(bytes).ok()?;
    (file.architecture() == architecture && file.mach_uuid().ok()? == Some(uuid) && has_dwarf(&file)).then_some(bytes)
}

/// Architecture variants can share object::Architecture, so test UUID on every slice.
fn matching_slice(bytes: &[u8], uuid: [u8; 16], architecture: object::Architecture) -> Option<&[u8]>
{
    match object::FileKind::parse(bytes).ok()? {
        object::FileKind::MachOFat32 => MachOFatFile32::parse(bytes)
            .ok()?
            .arches()
            .iter()
            .filter(|arch| arch.architecture() == architecture)
            .find_map(|arch| valid_slice(arch.data(bytes).ok()?, uuid, architecture)),
        object::FileKind::MachOFat64 => MachOFatFile64::parse(bytes)
            .ok()?
            .arches()
            .iter()
            .filter(|arch| arch.architecture() == architecture)
            .find_map(|arch| valid_slice(arch.data(bytes).ok()?, uuid, architecture)),
        _ => valid_slice(bytes, uuid, architecture),
    }
}

fn companion(path: &Path, uuid: [u8; 16], architecture: object::Architecture) -> Option<Vec<u8>>
{
    let bytes = fs::read(path).ok()?;
    matching_slice(&bytes, uuid, architecture).map(<[u8]>::to_vec)
}

/// Returns the actual bytes containing DWARF; executable metadata stays with the caller.
pub(crate) fn discover(path: &Path, executable: &[u8], options: &DebugInfoOptions) -> Result<Vec<u8>>
{
    let file = object::File::parse(executable)
        .map_err(|error| DebuggerError::InvalidArgument(format!("failed to parse {}: {error}", path.display())))?;
    if has_dwarf(&file) {
        return Ok(executable.to_vec());
    }
    let uuid = file
        .mach_uuid()
        .map_err(|error| no_debug_info(path, format!("cannot read executable UUID: {error}")))?
        .ok_or_else(|| no_debug_info(path, "executable has no Mach-O UUID for companion lookup"))?;
    let architecture = file.architecture();

    let mut bundle = path.as_os_str().to_os_string();
    bundle.push(".dSYM");
    let dwarf_directory = PathBuf::from(bundle).join("Contents/Resources/DWARF");
    if let Ok(entries) = fs::read_dir(dwarf_directory) {
        // UUID and architecture, not the DWARF member's name, establish identity.
        for entry in entries.flatten() {
            if let Some(bytes) = companion(&entry.path(), uuid, architecture) {
                return Ok(bytes);
            }
        }
    }

    let uuid_key: String = uuid.iter().map(|byte| format!("{byte:02x}")).collect();
    let cache_path = options.cache_directory.join(format!("{uuid_key}-{architecture:?}.dwarf"));
    if let Some(bytes) = companion(&cache_path, uuid, architecture) {
        return Ok(bytes);
    }
    let tool = options
        .dsymutil
        .as_ref()
        .ok_or_else(|| no_debug_info(path, "no matching dSYM or cached DWARF; dsymutil generation is disabled"))?;
    fs::create_dir_all(&options.cache_directory)
        .map_err(|error| no_debug_info(path, format!("cannot create debug cache: {error}")))?;
    let workspace = Workspace::create(&options.cache_directory)
        .map_err(|error| no_debug_info(path, format!("cannot create temporary debug cache: {error}")))?;
    let output = workspace.0.join("symbols.dwarf");
    let stderr_path = workspace.0.join("stderr");
    let stderr = File::create(&stderr_path).map_err(|error| no_debug_info(path, error.to_string()))?;
    let mut child = Command::new(tool)
        .arg("--flat")
        .arg("--out")
        .arg(&output)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|error| no_debug_info(path, format!("cannot run {}: {error}", tool.display())))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut detail = String::new();
                    if let Ok(stderr) = File::open(&stderr_path) {
                        let _ = stderr.take(4096).read_to_string(&mut detail);
                    }
                    return Err(no_debug_info(path, format!("dsymutil failed ({status}): {}", detail.trim())));
                }
                break;
            }
            Ok(None) if started.elapsed() < options.tool_timeout => thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(no_debug_info(
                    path,
                    match result {
                        Err(error) => format!("cannot wait for dsymutil: {error}"),
                        _ => format!("dsymutil timed out after {:?}", options.tool_timeout),
                    },
                ));
            }
        }
    }
    let bytes = companion(&output, uuid, architecture).ok_or_else(|| {
        no_debug_info(
            path,
            "dsymutil produced no usable DWARF with the executable's UUID and architecture",
        )
    })?;
    // The temporary file is on the same filesystem. Readers see either the previous
    // entry or a fully generated and validated file, including concurrent generation.
    if let Err(error) = fs::rename(&output, &cache_path)
        && companion(&cache_path, uuid, architecture).is_none()
    {
        return Err(no_debug_info(path, format!("cannot publish debug cache: {error}")));
    }
    Ok(bytes)
}

struct Workspace(PathBuf);

impl Workspace
{
    fn create(root: &Path) -> std::io::Result<Self>
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = root.join(format!(
                ".build-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for Workspace
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    // A minimal little-endian arm64 Mach-O, sufficient to exercise portable discovery
    // without executing a tool or requiring a native compiler.
    fn image(uuid: [u8; 16], debug: bool) -> Vec<u8>
    {
        let names = ["__debug_info", "__debug_abbrev", "__debug_line"];
        let commands_size = 24 + if debug { 72 + 80 * names.len() } else { 0 };
        let mut bytes = vec![0; 32 + commands_size + usize::from(debug) * 3];
        let word = |bytes: &mut [u8], offset, value: u32| bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        word(&mut bytes, 0, 0xfeed_facf);
        word(&mut bytes, 4, 0x0100_000c);
        word(&mut bytes, 12, 2);
        word(&mut bytes, 16, if debug { 2 } else { 1 });
        word(&mut bytes, 20, u32::try_from(commands_size).unwrap());
        word(&mut bytes, 32, 0x1b);
        word(&mut bytes, 36, 24);
        bytes[40..56].copy_from_slice(&uuid);
        if debug {
            let segment = 56;
            word(&mut bytes, segment, 0x19);
            word(&mut bytes, segment + 4, 72 + 80 * 3);
            bytes[segment + 8..segment + 15].copy_from_slice(b"__DWARF");
            word(&mut bytes, segment + 64, 3);
            for (index, name) in names.iter().enumerate() {
                let section = segment + 72 + 80 * index;
                bytes[section..section + name.len()].copy_from_slice(name.as_bytes());
                bytes[section + 16..section + 23].copy_from_slice(b"__DWARF");
                bytes[section + 40..section + 48].copy_from_slice(&1_u64.to_le_bytes());
                word(&mut bytes, section + 48, u32::try_from(32 + commands_size + index).unwrap());
            }
        }
        bytes
    }

    fn options(root: &Path) -> DebugInfoOptions
    {
        DebugInfoOptions {
            cache_directory: root.join("cache"),
            dsymutil: None,
            tool_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn embedded_dwarf_needs_neither_uuid_lookup_nor_tool()
    {
        let workspace = Workspace::create(&std::env::temp_dir()).unwrap();
        let bytes = image([1; 16], true);
        assert_eq!(
            discover(&workspace.0.join("program"), &bytes, &options(&workspace.0)).unwrap(),
            bytes
        );
    }

    #[test]
    fn adjacent_companion_must_match_uuid_and_architecture()
    {
        let workspace = Workspace::create(&std::env::temp_dir()).unwrap();
        let executable = workspace.0.join("program");
        let dwarf = workspace.0.join("program.dSYM/Contents/Resources/DWARF");
        fs::create_dir_all(&dwarf).unwrap();
        let member = dwarf.join("arbitrary-member-name");
        fs::write(&member, image([2; 16], true)).unwrap();
        assert!(matches!(
            discover(&executable, &image([1; 16], false), &options(&workspace.0)),
            Err(DebuggerError::NoDebugInfo { .. })
        ));
        let matching = image([1; 16], true);
        fs::write(&member, &matching).unwrap();
        assert_eq!(
            discover(&executable, &image([1; 16], false), &options(&workspace.0)).unwrap(),
            matching
        );
        let mut wrong_architecture = matching;
        wrong_architecture[4..8].copy_from_slice(&0x0100_0007_u32.to_le_bytes());
        fs::write(member, wrong_architecture).unwrap();
        assert!(matches!(
            discover(&executable, &image([1; 16], false), &options(&workspace.0)),
            Err(DebuggerError::NoDebugInfo { .. })
        ));
    }

    #[test]
    fn universal_companion_searches_later_same_architecture_slices()
    {
        let workspace = Workspace::create(&std::env::temp_dir()).unwrap();
        let wrong = image([2; 16], true);
        let mut matching = image([1; 16], true);
        matching[8..12].copy_from_slice(&2_u32.to_le_bytes()); // arm64e subtype
        let offsets = [256, (256 + wrong.len()).next_multiple_of(8)];
        let mut fat = vec![0; offsets[1] + matching.len()];
        fat[..4].copy_from_slice(&0xcafe_babe_u32.to_be_bytes());
        fat[4..8].copy_from_slice(&2_u32.to_be_bytes());
        for (index, data) in [wrong, matching.clone()].iter().enumerate() {
            let header = 8 + index * 20;
            for (offset, value) in [
                (0, 0x0100_000c),
                (4, if index == 0 { 0 } else { 2 }),
                (8, offsets[index] as u32),
                (12, data.len() as u32),
                (16, 3),
            ] {
                fat[header + offset..header + offset + 4].copy_from_slice(&value.to_be_bytes());
            }
            fat[offsets[index]..offsets[index] + data.len()].copy_from_slice(data);
        }
        let path = workspace.0.join("universal.dwarf");
        fs::write(&path, fat).unwrap();
        assert_eq!(companion(&path, [1; 16], object::Architecture::Aarch64), Some(matching));
    }

    #[test]
    fn cache_is_validated_and_can_be_used_without_a_tool()
    {
        let workspace = Workspace::create(&std::env::temp_dir()).unwrap();
        let options = options(&workspace.0);
        fs::create_dir_all(&options.cache_directory).unwrap();
        let cache = options.cache_directory.join(format!("{}-Aarch64.dwarf", "01".repeat(16)));
        fs::write(&cache, image([2; 16], true)).unwrap();
        assert!(discover(&workspace.0.join("program"), &image([1; 16], false), &options).is_err());
        let matching = image([1; 16], true);
        fs::write(cache, &matching).unwrap();
        assert_eq!(
            discover(&workspace.0.join("program"), &image([1; 16], false), &options).unwrap(),
            matching
        );
    }

    #[test]
    fn unavailable_tool_yields_typed_actionable_error_and_cleans_workspace()
    {
        let workspace = Workspace::create(&std::env::temp_dir()).unwrap();
        let mut options = options(&workspace.0);
        options.dsymutil = Some(workspace.0.join("missing-dsymutil"));
        let error = discover(&workspace.0.join("program"), &image([1; 16], false), &options).unwrap_err();
        assert!(matches!(error, DebuggerError::NoDebugInfo { .. }));
        assert!(error.to_string().contains("split-debuginfo"));
        assert!(error.to_string().contains("dsymutil"));
        assert_eq!(fs::read_dir(options.cache_directory).unwrap().count(), 0);
    }
}
