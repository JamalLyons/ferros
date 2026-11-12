//! # Loaded Images
//!
//! The executable images mapped into the target, for
//! [`Backend::images`](ferros_core::Backend::images).
//!
//! For now only the main executable is reported: its path comes from `libproc`
//! (`proc_pidpath`), and its load address is the address of its Mach-O header in the
//! target, found by scanning the readable memory regions for a 64-bit Mach-O header of
//! file type `MH_EXECUTE`. Enumerating dyld's image list (shared libraries) is a later
//! task (issue #34).

use std::path::PathBuf;

use ferros_core::backend::LoadedImage;
use ferros_core::error::Result;
use ferros_core::types::{MemoryRegion, ProcessId};
use libc::mach_port_t;

use crate::memory::read_memory;

/// `MH_MAGIC_64`: magic number of a 64-bit Mach-O header (host byte order).
const MH_MAGIC_64: u32 = 0xfeed_facf;
/// `MH_EXECUTE`: Mach-O file type of a main executable.
const MH_EXECUTE: u32 = 2;

/// Path of the executable of `pid`, from `libproc`.
pub(crate) fn executable_path(pid: ProcessId) -> Option<PathBuf>
{
    use libproc::libproc::proc_pid::pidpath;
    pidpath(pid.0 as i32).ok().map(PathBuf::from)
}

/// Whether `header` starts a 64-bit Mach-O main executable.
fn is_executable_header(header: &[u8]) -> bool
{
    let word = |offset: usize| {
        header
            .get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
    };
    word(0) == Some(MH_MAGIC_64) && word(12) == Some(MH_EXECUTE)
}

/// Address of the main executable's Mach-O header among `regions`, if one is found.
pub(crate) fn executable_load_address(task: mach_port_t, regions: &[MemoryRegion]) -> Option<u64>
{
    regions
        .iter()
        .filter(|region| region.permissions.contains('r'))
        .map(|region| region.start)
        .find(|&start| read_memory(task, start, 16).is_ok_and(|header| is_executable_header(&header)))
        .map(|start| start.value())
}

/// Images loaded in the target: currently the main executable only.
///
/// Returns an empty list if the executable's path or header cannot be found.
pub(crate) fn loaded_images(task: mach_port_t, pid: ProcessId, regions: &[MemoryRegion]) -> Result<Vec<LoadedImage>>
{
    let Some(path) = executable_path(pid) else {
        tracing::warn!("Could not get the executable path of process {}", pid.0);
        return Ok(Vec::new());
    };
    let Some(load_address) = executable_load_address(task, regions) else {
        tracing::warn!("No Mach-O executable header found in process {}", pid.0);
        return Ok(Vec::new());
    };
    Ok(vec![LoadedImage {
        path,
        load_address: load_address.into(),
    }])
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn recognises_only_64_bit_executable_headers()
    {
        let mut header = [0_u8; 16];
        header[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
        header[12..16].copy_from_slice(&MH_EXECUTE.to_le_bytes());
        assert!(is_executable_header(&header));

        // MH_DYLINKER (dyld) is not the main executable.
        header[12..16].copy_from_slice(&7_u32.to_le_bytes());
        assert!(!is_executable_header(&header));

        assert!(!is_executable_header(&[0_u8; 16]));
        assert!(!is_executable_header(&header[..8]));
    }
}
