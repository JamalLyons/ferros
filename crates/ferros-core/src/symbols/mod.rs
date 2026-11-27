//! # Symbol Resolution and DWARF Introspection
//!
//! This module provides symbol resolution, DWARF parsing, and type introspection
//! for debugging Rust programs. It uses the `gimli` crate to parse DWARF debugging
//! information and the `addr2line` crate for address-to-line mapping.
//!
//! ## Module Structure
//!
//! - **`cache`**: Symbol cache for binary images and address symbolication
//! - **`demangle`**: Symbol demangling utilities (Rust, C++)
//! - **`extractor`**: DWARF type extraction and introspection
//! - **`functions`**: Function index: function name to entry and post-prologue addresses
//! - **`image`**: Binary images: DWARF sections, segment mapping and unwind information
//!   read by the session's [`ObjectFormat`](crate::objfile::ObjectFormat)
//! - **`lines`**: Line-table index: `file:line` to addresses, address to line and ranges
//! - **`unwind`**: Stack unwinding: call frame information, unwind hints, frame-pointer chain
//!
//! ## DWARF Sections
//!
//! The module reads these DWARF sections, which the object format finds in an image or
//! its companion files:
//!
//! - **`.debug_info`**: Debugging information entries (DIEs)
//! - **`.debug_line`**: Line number information
//! - **`.debug_abbrev`**: Abbreviation tables
//! - **`.debug_str`**: String tables
//! - **`.debug_frame`**: Call frame information (CFI)
//!
//! ## References
//!
//! - [DWARF Debugging Information Format](https://dwarfstd.org/)
//! - [gimli crate documentation](https://docs.rs/gimli/0.32.3/gimli/)
//! - [addr2line crate documentation](https://docs.rs/addr2line/0.25.1/addr2line/)

use std::borrow::Cow;

use gimli::{Dwarf, EndianArcSlice, RunTimeEndian};

pub mod cache;
pub mod demangle;
pub mod extractor;
pub mod functions;
pub mod image;
pub mod lines;
pub(crate) mod load_inputs;
pub mod unwind;

// Shared type aliases
pub(crate) type OwnedReader = EndianArcSlice<RunTimeEndian>;
pub(crate) type OwnedDwarf = Dwarf<OwnedReader>;

/// A readable name makes this addr2line entry a logical stack frame.
pub(crate) fn dwarf_frame_name<'a>(frame: &'a addr2line::Frame<'_, OwnedReader>) -> Option<Cow<'a, str>>
{
    frame.function.as_ref()?.raw_name().ok()
}

// Re-exports
pub use cache::{SymbolCache, SymbolFrame, Symbolication};
pub use extractor::{TypeField, TypeKind, TypeSummary, TypeVariant};
pub use functions::FunctionAddress;
pub use image::{BinaryImage, ImageDescriptor, ImageId, ImageMapping, ImageSegment};
pub use lines::{AddressLine, LineAddresses, SkippedDebugInfo, SourceLookupError};
