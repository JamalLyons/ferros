//! Stamps of the files a symbol load reads, so a failed load is retried only once one
//! of them changes.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

/// What can be observed of the files one symbol load reads, without reading them: the
/// image itself and the companion files and directories its format consults
/// ([`ObjectFormat::load_inputs`](crate::objfile::ObjectFormat::load_inputs)). Two
/// equal stamps mean none of those files was created, removed, resized or modified in
/// between, as far as the filesystem's size and timestamps tell.
///
/// A file rewritten in place with the same size and its timestamps restored is not
/// seen as changed; neither is one changed within the filesystem's timestamp resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadInputs(Vec<(PathBuf, FileStamp)>);

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
        })
    }
}

impl LoadInputs
{
    /// Stamp `paths`, the files and directories one load of an image reads.
    pub(crate) fn of(paths: impl IntoIterator<Item = PathBuf>) -> Self
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
}
