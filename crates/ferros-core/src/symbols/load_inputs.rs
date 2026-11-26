//! Stamps of the files a symbol load reads, so a failed load is retried only once one
//! of them changes.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

/// What can be observed of the files one symbol load reads, without reading them: the
/// image itself and the companion files and directories its format consults. Two
/// equal stamps mean none of those files was created, removed, resized or modified in
/// between, as far as the filesystem's size, timestamps and file identity tell.
///
/// Where the filesystem reports a status-change time and a file number, both are
/// stamped too: a rewrite that keeps the size and restores the modification time still
/// moves the status-change time, which a process cannot set, and a file replaced by
/// another keeps neither number (#346). Elsewhere, a same-size rewrite with restored
/// timestamps is not seen as changed. A change within the filesystem's timestamp
/// resolution is not seen on any filesystem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadInputs(Vec<(PathBuf, FileStamp)>);

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

/// What a filesystem reports beyond size and the settable timestamps: the device and
/// file number, and the status-change time in nanoseconds since the epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity
{
    device: u64,
    file: u64,
    changed: (i64, i64),
}

impl FileIdentity
{
    #[cfg(unix)]
    fn of(metadata: &fs::Metadata) -> Option<Self>
    {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            device: metadata.dev(),
            file: metadata.ino(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    #[cfg(not(unix))]
    fn of(_: &fs::Metadata) -> Option<Self>
    {
        None
    }
}

impl MetadataStamp
{
    fn of(metadata: std::io::Result<fs::Metadata>) -> Option<Self>
    {
        let metadata = metadata.ok()?;
        Some(Self {
            is_dir: metadata.is_dir(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            identity: FileIdentity::of(&metadata),
        })
    }
}

impl LoadInputs
{
    /// Stamp `paths`, the files and directories one load of an image reads.
    pub fn of(paths: impl IntoIterator<Item = PathBuf>) -> Self
    {
        Self(
            paths
                .into_iter()
                .map(|path| {
                    let stamp = FileStamp {
                        link: MetadataStamp::of(fs::symlink_metadata(&path)),
                        target: MetadataStamp::of(fs::metadata(&path)),
                    };
                    (path, stamp)
                })
                .collect(),
        )
    }

    pub(crate) fn refresh(&self) -> Self
    {
        Self::of(self.0.iter().map(|(path, _)| path.clone()))
    }
}
