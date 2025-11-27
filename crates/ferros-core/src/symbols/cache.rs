//! # Symbol Cache
//!
//! Caching layer for binary images and symbol resolution.
//!
//! This module provides a `SymbolCache` that caches parsed binary images and
//! their DWARF metadata to avoid re-parsing the same binaries multiple times.
//! It also provides address symbolication (mapping addresses to function names
//! and source locations).
//!
//! ## Usage
//!
//! ```rust,no_run
//! use ferros_core::symbols::{ImageDescriptor, SymbolCache};
//! use ferros_core::types::Address;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>>
//! {
//!     let mut cache = SymbolCache::new();
//!     let descriptor = ImageDescriptor {
//!         path: "/path/to/binary".into(),
//!         load_address: 0x100000000,
//!     };
//!
//!     // Load the binary image (parses DWARF sections)
//!     let _image = cache.load_image(descriptor)?;
//!
//!     // Symbolicate an address
//!     if let Some(symbolication) = cache.symbolicate(Address::from(0x100001000)) {
//!         for frame in symbolication.frames {
//!             println!("Function: {:?}", frame.symbol);
//!             if let Some(loc) = frame.location {
//!                 if let Some(line) = loc.line {
//!                     println!("Location: {}:{}", loc.file, line);
//!                 } else {
//!                     println!("Location: {}", loc.file);
//!                 }
//!             }
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use super::extractor::TypeSummary;
use super::image::{BinaryImage, ImageDescriptor, ImageId};
use crate::error::{DebuggerError, Result};
use crate::types::{Address, FunctionParameter, SourceLocation, SymbolName};

/// Cached symbol metadata for a given address.
///
/// This structure contains the result of symbolication (address-to-symbol mapping)
/// for a single address. It includes multiple frames to handle inlined functions.
#[derive(Debug, Clone)]
pub struct Symbolication
{
    /// ID of the binary image containing this address
    pub image_id: ImageId,
    /// Stack of symbol frames (outermost to innermost, for inlined functions)
    pub frames: Vec<SymbolFrame>,
}

/// A single symbol frame in a symbolication result.
///
/// Represents one level of the call stack at a given address, including
/// the function name and source location (if available).
#[derive(Debug, Clone)]
pub struct SymbolFrame
{
    /// Symbol name (mangled and demangled)
    pub symbol: SymbolName,
    /// Source location (file, line, column) if available
    pub location: Option<SourceLocation>,
    /// Function parameters (if available from DWARF)
    pub parameters: Vec<FunctionParameter>,
}

/// Cache for binary images and their DWARF metadata.
///
/// This cache stores parsed binary images to avoid re-parsing DWARF sections
/// multiple times. It provides efficient lookup of images by address and
/// symbolication of addresses to function names and source locations.
///
/// ## Thread Safety
///
/// The cache is not thread-safe. If you need concurrent access, wrap it in
/// a `Mutex` or `RwLock`.
#[derive(Default)]
pub struct SymbolCache
{
    images: HashMap<ImageId, Arc<BinaryImage>>,
}

impl SymbolCache
{
    /// Create a new empty symbol cache.
    #[must_use]
    pub fn new() -> Self
    {
        Self { images: HashMap::new() }
    }

    /// Load a binary image and parse its DWARF sections.
    ///
    /// This method:
    /// 1. Canonicalizes the image path
    /// 2. Checks if the image is already cached
    /// 3. Parses the binary and loads DWARF sections
    /// 4. Caches the parsed image for future lookups
    ///
    /// ## Parameters
    ///
    /// - `descriptor`: Image descriptor with path and load address
    ///
    /// ## Returns
    ///
    /// An `Arc<BinaryImage>` containing the parsed binary and DWARF metadata.
    /// If the image was already cached, returns the cached version.
    ///
    /// ## Errors
    ///
    /// Returns an error if:
    /// - The image path cannot be canonicalized
    /// - The binary cannot be parsed
    /// - DWARF sections cannot be loaded
    pub fn load_image(&mut self, descriptor: ImageDescriptor) -> Result<Arc<BinaryImage>>
    {
        let canonical = if descriptor.path.is_absolute() {
            descriptor.path.clone()
        } else {
            descriptor.path.canonicalize().map_err(|err| {
                DebuggerError::InvalidArgument(format!("unable to canonicalize {}: {err}", descriptor.path.display()))
            })?
        };

        let id = ImageId::from_parts(&canonical, descriptor.load_address);
        if let Some(existing) = self.images.get(&id) {
            return Ok(existing.clone());
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let image = Arc::new(BinaryImage::parse(ImageDescriptor {
            path: canonical,
            load_address: descriptor.load_address,
        })?);
        self.images.insert(id, image.clone());
        Ok(image)
    }

    /// Find the binary image containing the given address.
    ///
    /// Searches through all cached images to find one that contains the address
    /// (i.e., the address is within the image's load address range).
    ///
    /// ## Returns
    ///
    /// `Some(image)` if an image containing the address is found, `None` otherwise.
    pub fn image_for_address(&self, address: Address) -> Option<Arc<BinaryImage>>
    {
        self.images.values().find(|image| image.contains(address)).cloned()
    }

    /// Symbolicate an address to function names and source locations.
    ///
    /// This method maps an address to its corresponding function name and source
    /// location using DWARF line information. It handles inlined functions by returning
    /// multiple frames (outermost to innermost).
    ///
    /// ## Parameters
    ///
    /// - `address`: The address to symbolicate
    ///
    /// ## Returns
    ///
    /// `Some(symbolication)` if the address is found in a cached image, `None` otherwise.
    pub fn symbolicate(&self, address: Address) -> Option<Symbolication>
    {
        self.image_for_address(address).and_then(|image| image.symbolicate(address))
    }

    /// Describe a type by name using DWARF type information.
    ///
    /// Searches through all cached images to find type definitions matching the
    /// given name. Returns a summary of the type's structure (fields, variants, etc.).
    ///
    /// ## Parameters
    ///
    /// - `name`: The type name to look up (e.g., "std::option::Option")
    ///
    /// ## Returns
    ///
    /// `Ok(Some(summary))` if the type is found, `Ok(None)` if not found,
    /// or an error if DWARF parsing fails.
    pub fn describe_type(&self, name: &str) -> Result<Option<Arc<TypeSummary>>>
    {
        for image in self.images.values() {
            if let Some(summary) = image.describe_type(name)? {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }
}
