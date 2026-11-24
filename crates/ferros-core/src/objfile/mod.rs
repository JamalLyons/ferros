//! # Object files without naming a format
//!
//! The engine asks container questions about the images mapped in a target: how an
//! image's file addresses map to runtime addresses, which build it is, where its symbol
//! table is, which addresses are its code, whether memory that no loader lists holds an
//! image, and what debug and unwind information its file on disk holds. The answers depend
//! on the object-file format, which `ferros-core` does not know. [`ObjectFormat`] is the
//! question set; each platform crate implements it for its own format and the session is
//! given one ([`Session::new`](crate::Session::new)).
//!
//! The format never sees a [`Backend`](crate::Backend): it reads the target through
//! [`TargetMemory`], which the session adapts over its backend. The session keeps the
//! policy: which images to ask about, what to cache and for how long, when a failed read
//! is retried, and which memory regions to search for an unlisted image. The format only
//! recognizes and parses its own bytes.
//!
//! [`ObjectFormat`] is not part of [`Backend`](crate::Backend). Process control and
//! container parsing stay separate (ADR 0001), and a test that drives
//! [`MockBackend`](crate::backend::mock::MockBackend) uses
//! [`mock::MockObjectFormat`] for symbol tables without any format's bytes.
//!
//! ## Symbol tables
//!
//! Code without DWARF (a C runtime shim, a system library) still has a symbol table
//! entry, and the nearest symbol at or before a pc names the function containing it. A
//! format finds an image's table from its mapped header with
//! [`ObjectFormat::mapped_symbols`], which returns a [`MappedSymbols`]: the image's code
//! ranges and how to read the table. [`MappedSymbols::load`] reads the table itself into a
//! [`SymbolIndex`], and [`SymbolIndex::read_name`] reads one name. Each step reads only
//! what it needs, so the session can keep what was read and retry only what failed.

mod contents;
#[cfg(any(test, feature = "test-util"))]
pub mod mock;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use gimli::{RunTimeEndian, SectionId};

pub use self::contents::{
    DWARF_SECTIONS, DebugSections, FunctionUnwind, ImageContents, ImageSlice, MappedImage, SectionData, UnwindHint,
    UnwindHints,
};
pub use crate::symbols::image::{ImageMapping, ImageSegment};
use crate::types::Address;

/// Reads of the target's memory, for an [`ObjectFormat`].
///
/// The session implements it over its backend. Reads return the target's bytes as
/// mapped, with no software breakpoint hidden: an image's header and symbol table never
/// hold a trap.
pub trait TargetMemory
{
    /// Read `len` bytes at `at`.
    ///
    /// # Errors
    /// A description of why the memory could not be read (for example, it is not mapped).
    fn read(&mut self, at: Address, len: usize) -> Result<Vec<u8>, String>;
}

/// One object-file format's answers to the engine's container questions.
///
/// See the [module documentation](self). An implementor knows one format and is shared
/// by the session (`Arc<dyn ObjectFormat>`), so it holds no per-process state.
pub trait ObjectFormat: Send + Sync
{
    /// The symbol table of the image whose header is mapped at `load_address`.
    ///
    /// # Errors
    /// A description of why the header could not be read or parsed, or why it has no
    /// usable symbol table. The session logs it and asks again in a later request.
    fn mapped_symbols(&self, memory: &mut dyn TargetMemory, load_address: Address)
    -> Result<Arc<dyn MappedSymbols>, String>;

    /// The alignment at which this format's images can start in memory no loader lists,
    /// or `None` (the default) when the format has no such images to search for.
    ///
    /// The session searches back from a pc without a listed image, one step of this size
    /// at a time and within the pc's executable region, calling
    /// [`probe_unlisted_image`](Self::probe_unlisted_image) at each step. It must be a
    /// power of two.
    fn unlisted_image_alignment(&self) -> Option<u64>
    {
        None
    }

    /// Whether an image header starts at `at`, an address the session chose on
    /// [`unlisted_image_alignment`](Self::unlisted_image_alignment). The default finds
    /// nothing.
    ///
    /// A candidate that is not this format's header, or does not parse, is
    /// [`ImageProbe::Nothing`]; the session keeps searching below it.
    fn probe_unlisted_image(&self, memory: &mut dyn TargetMemory, at: Address) -> ImageProbe
    {
        let _ = (memory, at);
        ImageProbe::Nothing
    }

    /// The segment layout and slice of the image whose header is mapped at
    /// `load_address`, read from the target only.
    ///
    /// A header that cannot be read or parsed is a [`MappedImage`] whose `mapping` holds
    /// the reason and whose `slice` is `None`. The session keeps the result while the
    /// image stays loaded as the same build.
    fn map_image(&self, memory: &mut dyn TargetMemory, load_address: Address) -> MappedImage;

    /// Read the file at `path`, mapped at `load_address`, for its unwind and debug
    /// information. `slice` is the [`MappedImage::slice`] the session read from the
    /// mapped header, with any build id the loader reported since; `None` when it is
    /// unknown. A format whose files hold several images reads the one `slice` names, and
    /// a file of another build than `slice` names is an error.
    ///
    /// The image file's layout and unwind tables are returned whether or not debug
    /// information is found: a missing or unreadable companion debug file is
    /// [`ImageContents::debug`], not an error of this call, so the session can still
    /// unwind through the image.
    ///
    /// # Errors
    /// [`DebuggerError::NoDebugInfo`](crate::DebuggerError::NoDebugInfo) when the format
    /// cannot tell which part of the file is mapped (the session keeps it while the image
    /// stays loaded); any other error for a file that cannot be read, does not parse, or is
    /// another build (the session retries once [`load_inputs`](Self::load_inputs)
    /// change).
    fn open_image(&self, path: &Path, load_address: u64, slice: Option<&ImageSlice>) -> crate::Result<ImageContents>;

    /// The files [`open_image`](Self::open_image) reads for the image at `path`, `path`
    /// included, in a stable order: companion debug files and any cache directory the
    /// format consults. The session stamps them (size and timestamps) when a read fails
    /// and reads again only once one of them changes. The default is `path` alone.
    fn load_inputs(&self, path: &Path) -> Vec<PathBuf>
    {
        vec![path.to_path_buf()]
    }
}

/// What [`ObjectFormat::probe_unlisted_image`] found at one address.
#[derive(Clone)]
pub enum ImageProbe
{
    /// No usable image header starts here.
    Nothing,
    /// The address could not be read; a search stops here.
    Unreadable,
    /// An image header whose symbol table was located.
    Image(Arc<dyn MappedSymbols>),
}

impl std::fmt::Debug for ImageProbe
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        match self {
            Self::Nothing => f.write_str("Nothing"),
            Self::Unreadable => f.write_str("Unreadable"),
            Self::Image(_) => f.write_str("Image(..)"),
        }
    }
}

/// Where one mapped image's code and symbol table lie in the target, read from its
/// header ([`ObjectFormat::mapped_symbols`]).
pub trait MappedSymbols: Send + Sync
{
    /// Whether `address` lies in the image's code (its executable sections). A pc
    /// outside it is never named from this image.
    fn contains(&self, address: Address) -> bool;

    /// Read the image's symbol table from the target.
    ///
    /// # Errors
    /// A description of the failed read. The session retries it in a later request.
    fn load(&self, memory: &mut dyn TargetMemory) -> Result<Box<dyn SymbolIndex>, String>;

    /// The function name a raw symbol table name stands for. The default is the raw name;
    /// a format that decorates names (some prefix C-level names with `_`) removes the
    /// decoration here.
    fn function_name<'a>(&self, raw: &'a str) -> &'a str
    {
        raw
    }
}

/// An image's code symbols, sorted by runtime address ([`MappedSymbols::load`]).
pub trait SymbolIndex: Send + Sync
{
    /// The symbol naming the function that contains `address`, or `None` when no symbol
    /// does (outside the code, before the first symbol, or in a function the table does
    /// not name).
    fn lookup(&self, address: Address) -> Option<SymbolKey>;

    /// Every code symbol, `(runtime address, key)`, sorted by address.
    fn symbols(&self) -> Box<dyn Iterator<Item = (Address, SymbolKey)> + '_>;

    /// Read the raw name of `key` from the target. `Ok(None)` for an empty or non-UTF-8
    /// name, or a key outside the table.
    ///
    /// # Errors
    /// A description of the failed read. The session retries it on the next lookup.
    fn read_name(&self, memory: &mut dyn TargetMemory, key: SymbolKey) -> Result<Option<String>, String>;
}

/// Identifies one symbol of a [`SymbolIndex`], for reading and caching its name. Its
/// value means something only to the index that returned it (a string table index, for
/// example).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolKey(pub u64);
