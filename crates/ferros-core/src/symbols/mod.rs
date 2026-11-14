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
//! - **`image`**: Binary image parsing and DWARF section loading
//! - **`lines`**: Line-table index: `file:line` to addresses, address to line and ranges
//!
//! ## DWARF Sections
//!
//! The module loads DWARF sections from binary images:
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

use gimli::{Dwarf, EndianArcSlice, RunTimeEndian};

pub mod cache;
pub mod debuginfo;
pub mod demangle;
pub mod extractor;
pub mod functions;
pub mod image;
pub mod lines;
pub mod unwind;

// Shared type aliases
pub(crate) type OwnedReader = EndianArcSlice<RunTimeEndian>;
pub(crate) type OwnedDwarf = Dwarf<OwnedReader>;

// Re-exports
pub use cache::{SymbolCache, SymbolFrame, Symbolication};
pub use debuginfo::DebugInfoOptions;
pub use extractor::{TypeField, TypeKind, TypeSummary, TypeVariant};
pub use functions::FunctionAddress;
pub use image::{BinaryImage, ImageDescriptor, ImageId, ImageMapping};
pub use lines::{AddressLine, LineAddresses, SourceLookupError};
