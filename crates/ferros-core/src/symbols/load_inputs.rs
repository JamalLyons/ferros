//! Stamps of the files a symbol load reads, so a failed load is retried only once one
//! of them changes.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

/// What can be observed of the files one symbol load reads, without reading them: the
/// image itself and the companion files and directories its format consults. Two
/// equal stamps mean none of those files was created, removed, resized or modified in
/// between, as far as size, timestamps and the host's file identity tell.
///
/// The object format may supply a [`FileIdentityReader`]. Where it does, a rewrite that
/// keeps the size and restores the modification time is still a different stamp when the
/// reader reports a new identity: a status-change time a process cannot set, or a new
/// file number (#346). Without a reader, that rewrite is not seen. A change within the
/// filesystem's timestamp resolution is not seen on any host.
#[derive(Clone, Debug, Default)]
pub struct LoadInputs
{
    stamps: Vec<(PathBuf, FileStamp)>,
    identity: Option<FileIdentityReader>,
}

/// One path's stamp: the link itself and what it resolves to (equal for a regular
/// file), so retargeting a symlink is seen too. `None` when the path does not exist or
/// cannot be inspected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp
{
    link: Option<MetadataStamp>,
    target: Option<MetadataStamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MetadataStamp
{
    is_dir: bool,
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    identity: Option<FileIdentity>,
}

/// Identity of one file beyond its size and the timestamps a process can set.
///
/// A platform crate builds this from what its filesystem reports. Core compares it and
/// does not know how to read it. `device` and `number` name the file; `changed_secs`
/// and `changed_nanos` are its status-change time since the epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity
{
    device: u64,
    number: u64,
    changed_secs: i64,
    changed_nanos: i64,
}

impl FileIdentity
{
    /// An identity with these filesystem fields.
    #[must_use]
    pub const fn new(device: u64, number: u64, changed_secs: i64, changed_nanos: i64) -> Self
    {
        Self {
            device,
            number,
            changed_secs,
            changed_nanos,
        }
    }
}

/// Reads a [`FileIdentity`] from filesystem metadata.
///
/// The platform crate that implements [`ObjectFormat`](crate::objfile::ObjectFormat)
/// supplies this. Core stores the result on a stamp and compares it later.
pub type FileIdentityReader = fn(&fs::Metadata) -> Option<FileIdentity>;

impl MetadataStamp
{
    fn of(metadata: std::io::Result<fs::Metadata>, identity: Option<FileIdentityReader>) -> Option<Self>
    {
        let metadata = metadata.ok()?;
        Some(Self {
            is_dir: metadata.is_dir(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            identity: identity.and_then(|read| read(&metadata)),
        })
    }
}

impl PartialEq for LoadInputs
{
    fn eq(&self, other: &Self) -> bool
    {
        self.stamps == other.stamps
    }
}

impl Eq for LoadInputs {}

impl LoadInputs
{
    /// Stamp `paths`, the files and directories one load of an image reads.
    ///
    /// No host file identity is recorded. A rewrite that keeps the size and restores
    /// the modification time compares equal.
    pub fn of(paths: impl IntoIterator<Item = PathBuf>) -> Self
    {
        Self::with_identity(paths, None)
    }

    /// Stamp `paths`, recording whatever `identity` reports for each one.
    ///
    /// `identity` is kept and used again by a later refresh of the same paths. `None`
    /// records size and timestamps only.
    #[must_use]
    pub fn with_identity(paths: impl IntoIterator<Item = PathBuf>, identity: Option<FileIdentityReader>) -> Self
    {
        Self {
            stamps: paths
                .into_iter()
                .map(|path| {
                    let stamp = FileStamp {
                        link: MetadataStamp::of(fs::symlink_metadata(&path), identity),
                        target: MetadataStamp::of(fs::metadata(&path), identity),
                    };
                    (path, stamp)
                })
                .collect(),
            identity,
        }
    }

    pub(crate) fn refresh(&self) -> Self
    {
        Self::with_identity(self.stamps.iter().map(|(path, _)| path.clone()), self.identity)
    }
}

#[cfg(test)]
mod tests
{
    use std::io::Write;
    use std::time::Duration;

    use super::{FileIdentity, LoadInputs};

    fn sample(name: &str) -> std::path::PathBuf
    {
        let path = std::env::temp_dir().join(format!("ferros-load-inputs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn write_and_mtime(path: &std::path::Path, bytes: &[u8]) -> std::time::SystemTime
    {
        let mut file = std::fs::File::create(path).expect("create sample");
        file.write_all(bytes).expect("write sample");
        file.metadata().expect("sample metadata").modified().expect("sample mtime")
    }

    fn restore(path: &std::path::Path, bytes: &[u8], modified: std::time::SystemTime)
    {
        let mut file = std::fs::File::options().write(true).open(path).expect("reopen sample");
        file.write_all(bytes).expect("rewrite sample");
        file.set_modified(modified).expect("restore mtime");
    }

    fn fixed(_: &std::fs::Metadata) -> Option<FileIdentity>
    {
        Some(FileIdentity::new(1, 2, 3, 4))
    }

    fn moved(_: &std::fs::Metadata) -> Option<FileIdentity>
    {
        Some(FileIdentity::new(1, 2, 9, 0))
    }

    #[test]
    fn a_new_file_identity_is_seen_when_size_and_mtime_are_unchanged()
    {
        let path = sample("identity");
        let bytes = b"same-size-body";
        let modified = write_and_mtime(&path, bytes);
        let first = LoadInputs::with_identity([path.clone()], Some(fixed));

        restore(&path, bytes, modified);
        let metadata = std::fs::metadata(&path).expect("rewritten metadata");
        assert_eq!(metadata.len(), bytes.len() as u64);
        assert_eq!(metadata.modified().ok(), Some(modified));

        let same = LoadInputs::with_identity([path.clone()], Some(fixed));
        assert_eq!(first, same, "the same identity must not look like a change");
        assert_eq!(same, same.refresh(), "refresh uses the stored reader");

        let changed = LoadInputs::with_identity([path.clone()], Some(moved));
        assert_ne!(
            first, changed,
            "a new file identity must be seen when size and mtime are unchanged"
        );

        let plain = LoadInputs::of([path.clone()]);
        assert_eq!(
            plain,
            LoadInputs::of([path.clone()]),
            "without a reader, a same-size rewrite with its mtime restored is not seen"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_later_mtime_is_seen_without_a_file_identity()
    {
        let path = sample("mtime");
        let bytes = b"same-size-body";
        let modified = write_and_mtime(&path, bytes);
        let first = LoadInputs::of([path.clone()]);
        restore(&path, bytes, modified + Duration::from_secs(2));
        assert_ne!(first, LoadInputs::of([path.clone()]));
        let _ = std::fs::remove_file(&path);
    }
}
