//! What an [`ObjectFormat`](super::ObjectFormat) reads from a mapped header and from an
//! image's file, in terms the engine understands without knowing the format.

use std::collections::HashMap;
use std::sync::Arc;

use gimli::{RunTimeEndian, SectionId};

use crate::symbols::image::ImageMapping;
use crate::types::{Address, Architecture, BuildIdentity};

/// The DWARF sections the engine reads, in the order a format may look them up. A format
/// returns any of them it finds in [`DebugSections::dwarf`]; one it does not return reads
/// as empty.
pub const DWARF_SECTIONS: &[SectionId] = &[
    SectionId::DebugAbbrev,
    SectionId::DebugAddr,
    SectionId::DebugInfo,
    SectionId::DebugLine,
    SectionId::DebugLineStr,
    SectionId::DebugRanges,
    SectionId::DebugRngLists,
    SectionId::DebugStr,
    SectionId::DebugStrOffsets,
    SectionId::DebugTypes,
    SectionId::DebugLoc,
    SectionId::DebugLocLists,
    SectionId::DebugPubNames,
    SectionId::DebugPubTypes,
    SectionId::DebugFrame,
    SectionId::DebugMacro,
    SectionId::DebugCuIndex,
    SectionId::DebugTuIndex,
];

/// Which build of an image is mapped, and which part of its file holds it.
///
/// A format whose files can hold several images (one per architecture, for example)
/// fills `selector` with whatever names the mapped one; the engine never interprets it.
/// It is part of the symbol cache's key, so two slices of one file at one address are
/// cached apart, and it is passed back to
/// [`ObjectFormat::open_image`](super::ObjectFormat::open_image). `build_id` is the
/// build identity the mapped header records, completed by the loader's report when the
/// header's could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageSlice
{
    selector: Arc<[u8]>,
    build_id: BuildIdentity,
}

impl ImageSlice
{
    /// A slice named by the format-defined `selector`, of the build `build_id`.
    #[must_use]
    pub fn new(selector: impl Into<Arc<[u8]>>, build_id: BuildIdentity) -> Self
    {
        Self {
            selector: selector.into(),
            build_id,
        }
    }

    /// The format-defined bytes that name this slice.
    #[must_use]
    pub fn selector(&self) -> &[u8]
    {
        &self.selector
    }

    /// The slice's build identity.
    #[must_use]
    pub const fn build_id(&self) -> BuildIdentity
    {
        self.build_id
    }

    /// This slice with its build identity completed by a later report
    /// ([`BuildIdentity::or_later`]): an unknown id becomes a known one.
    #[must_use]
    pub fn or_later(mut self, later: BuildIdentity) -> Self
    {
        self.build_id = self.build_id.or_later(later);
        self
    }
}

/// What [`ObjectFormat::map_image`](super::ObjectFormat::map_image) read from one mapped
/// header.
#[derive(Debug, Clone)]
pub struct MappedImage
{
    /// The image's segment layout, or why the header could not be read or parsed.
    pub mapping: Result<ImageMapping, String>,
    /// Which slice and build is mapped; `None` when the header could not be read.
    pub slice: Option<ImageSlice>,
}

/// One section's bytes and its file (link-time) virtual address.
#[derive(Debug, Clone)]
pub struct SectionData
{
    /// The section's link-time address.
    pub address: u64,
    /// The section's bytes, uncompressed.
    pub data: Arc<[u8]>,
}

/// What [`ObjectFormat::open_image`](super::ObjectFormat::open_image) read of one image:
/// the image file's own layout and unwind information, and, apart from it, the source
/// debug information the format found for it.
///
/// The two halves are independent. An image whose file has unwind tables but no usable
/// debug information (no companion file was found) still unwinds: `debug` is then an
/// error and the rest is filled. Both halves describe the build the session asked for, at
/// the same load address.
pub struct ImageContents
{
    /// The image's architecture.
    pub architecture: Architecture,
    /// Byte order of the image's DWARF and CFI.
    pub endian: RunTimeEndian,
    /// The build identity the opened file records: [`BuildIdentity::Absent`] when it
    /// records none, [`BuildIdentity::Unknown`] when the format could not tell. When the
    /// file was opened while the mapped build was unknown and the loader reports the mapped
    /// id later, the session stops serving the file unless this can be the same build
    /// ([`same_build`](crate::types::same_build)).
    pub build_id: BuildIdentity,
    /// The file's segment layout at the load address.
    pub mapping: ImageMapping,
    /// `.eh_frame` (or the format's equivalent) of the image file itself, whose FDE
    /// ranges are runtime addresses once its own address is relocated.
    pub eh_frame: Option<SectionData>,
    /// `.eh_frame_hdr`, the binary-search table over `eh_frame`.
    pub eh_frame_hdr: Option<SectionData>,
    /// Per-function unwind hints of the format, consulted before the CFI.
    pub unwind_hints: Option<Box<dyn UnwindHints>>,
    /// The image's source debug information, from the file or a companion file the
    /// format found, or why there is none.
    ///
    /// [`DebuggerError::NoDebugInfo`](crate::DebuggerError::NoDebugInfo) when the image
    /// has no usable debug information; other errors for debug information that could
    /// not be read or parsed. The session checks the format's
    /// [`load_inputs`](super::ObjectFormat::load_inputs) before reusing either failure.
    pub debug: crate::Result<DebugSections>,
}

/// Re-read companion debug information for an already validated executable without
/// reparsing it. Returned by [`ObjectFormat::open_image_with_debug_retry`](super::ObjectFormat::open_image_with_debug_retry).
pub type DebugInfoRetry = Arc<dyn Fn() -> crate::Result<DebugSections> + Send + Sync>;

impl std::fmt::Debug for ImageContents
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        f.debug_struct("ImageContents")
            .field("architecture", &self.architecture)
            .field("endian", &self.endian)
            .field("build_id", &self.build_id)
            .field("mapping", &self.mapping)
            .field("eh_frame", &self.eh_frame.is_some())
            .field("eh_frame_hdr", &self.eh_frame_hdr.is_some())
            .field("unwind_hints", &self.unwind_hints.is_some())
            .field("debug", &self.debug)
            .finish()
    }
}

/// Source debug information of one image ([`ImageContents::debug`]): its DWARF, from the
/// image file or a companion debug file. The image file's own unwind tables are not here.
#[derive(Clone, Default)]
pub struct DebugSections
{
    /// DWARF sections by id (see [`DWARF_SECTIONS`]). A missing section reads as empty.
    pub dwarf: HashMap<SectionId, Arc<[u8]>>,
    /// `.debug_frame`, whose FDE ranges are file addresses.
    pub debug_frame: Option<SectionData>,
}

impl std::fmt::Debug for DebugSections
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        f.debug_struct("DebugSections")
            .field("dwarf", &self.dwarf.keys().collect::<Vec<_>>())
            .field("debug_frame", &self.debug_frame.is_some())
            .finish()
    }
}

/// A format's own per-function unwind table, as hints the unwinder applies before the
/// image's CFI (see [`symbols::unwind`](crate::symbols::unwind)).
pub trait UnwindHints: Send + Sync
{
    /// The hint for the function containing the runtime `address`, or `None` when the
    /// table has no usable entry for it: the unwinder then searches the CFI.
    fn lookup(&self, address: Address) -> Option<FunctionUnwind>;
}

/// The unwind hint of one function ([`UnwindHints::lookup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionUnwind
{
    /// Runtime address of the first instruction the hint covers. A table may give one
    /// entry to a run of adjacent functions; this is the first of them.
    pub start: Address,
    /// How the function's caller is found.
    pub hint: UnwindHint,
}

/// How to find a function's caller, as an [`UnwindHints`] table states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnwindHint
{
    /// The function's FDE is at this offset in the image's `eh_frame`.
    Cfi
    {
        /// Offset of the FDE in [`ImageContents::eh_frame`].
        eh_frame_offset: usize,
    },
    /// A leaf that saves neither the return address nor the frame pointer: the return
    /// address stays in the link register and the function allocates `stack_size` bytes
    /// below its caller's stack pointer.
    Leaf
    {
        /// Bytes allocated on the stack.
        stack_size: u64,
    },
    /// The function keeps a standard frame record at the frame pointer.
    FrameRecord,
}
