//! Device, file number and status-change time for one file.
//!
//! [`LoadInputs`](ferros_core::objfile::LoadInputs) compares this so a rewrite that keeps
//! the size and restores the modification time is still seen (#346). A process cannot set
//! the status-change time, and replacing the file changes its number.

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

use ferros_core::objfile::FileIdentity;

/// The file identity `metadata` reports: device, inode and status-change time.
#[must_use]
pub fn unix_file_identity(metadata: &Metadata) -> Option<FileIdentity>
{
    Some(FileIdentity::new(
        metadata.dev(),
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    ))
}

#[cfg(test)]
mod tests
{
    use std::io::{Seek, SeekFrom, Write};

    use super::unix_file_identity;

    #[test]
    fn a_rewrite_that_keeps_size_and_mtime_changes_the_identity()
    {
        let path = std::env::temp_dir().join(format!("ferros-file-identity-{}", std::process::id()));
        let bytes = b"same-size-body";
        let mut file = std::fs::File::create(&path).expect("create sample");
        file.write_all(bytes).expect("write sample");
        let before = file.metadata().expect("sample metadata");
        let modified = before.modified().expect("sample mtime");
        let first = unix_file_identity(&before).expect("identity");

        file.seek(SeekFrom::Start(0)).expect("rewind sample");
        file.write_all(b"SAME-SIZE-BODY").expect("rewrite sample");
        file.set_modified(modified).expect("restore mtime");
        let after = std::fs::metadata(&path).expect("rewritten metadata");
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().ok(), Some(modified));
        assert_ne!(
            unix_file_identity(&after),
            Some(first),
            "a same-size rewrite with its mtime restored kept the same identity"
        );
        let _ = std::fs::remove_file(&path);
    }
}
