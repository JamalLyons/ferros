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
//! # use ferros_core::objfile::mock::MockObjectFormat;
//! use ferros_core::symbols::{ImageDescriptor, SymbolCache};
//! use ferros_core::types::Address;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>>
//! {
//!     // A platform crate supplies the object format of its images.
//!     let mut cache = SymbolCache::new(MockObjectFormat::shared());
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::extractor::TypeSummary;
use super::image::{BinaryImage, ImageDescriptor, ImageId};
use super::load_inputs::LoadInputs;
use crate::error::{DebuggerError, Result};
use crate::objfile::{ImageSlice, ObjectFormat};
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
/// symbolication of addresses to function names and source locations. Image files are
/// read through the [`ObjectFormat`] the cache is given.
///
/// ## Thread Safety
///
/// The cache is not thread-safe. If you need concurrent access, wrap it in
/// a `Mutex` or `RwLock`.
pub struct SymbolCache
{
    images: HashMap<ImageId, Arc<BinaryImage>>,
    format: Arc<dyn ObjectFormat>,
}

impl SymbolCache
{
    /// Create an empty symbol cache reading image files as `format`.
    #[must_use]
    pub fn new(format: Arc<dyn ObjectFormat>) -> Self
    {
        Self {
            images: HashMap::new(),
            format,
        }
    }

    /// Drop metadata for images no longer reported by the loader.
    ///
    /// The ids are computed as [`load_image`](Self::load_image) computes them. To
    /// keep entries loaded with a slice, use [`retain_ids`](Self::retain_ids)
    /// with the returned images' IDs (#170, #175).
    pub fn retain_images(&mut self, descriptors: &[ImageDescriptor])
    {
        let ids: HashSet<_> = descriptors
            .iter()
            .filter_map(|descriptor| {
                let path = if descriptor.path.is_absolute() {
                    descriptor.path.clone()
                } else {
                    descriptor.path.canonicalize().ok()?
                };
                Some(ImageId::from_parts(&path, descriptor.load_address))
            })
            .collect();
        self.retain_ids(&ids);
    }

    /// Drop metadata for every image whose id is not in `ids`.
    ///
    /// Unlike [`retain_images`](Self::retain_images), this does not resolve paths again, so
    /// an image loaded from a relative path that no longer resolves is kept while its
    /// owner still reports it mapped.
    pub fn retain_ids(&mut self, ids: &HashSet<ImageId>)
    {
        self.images.retain(|id, _| ids.contains(id));
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
    ///
    /// The descriptor does not say which slice or build of the file the target mapped
    /// (see [`ImageSlice`]); a format whose files hold several images may report
    /// [`DebuggerError::NoDebugInfo`] for such a file. Use
    /// [`load_image_slice`](Self::load_image_slice) to name the slice (#175).
    pub fn load_image(&mut self, descriptor: ImageDescriptor) -> Result<Arc<BinaryImage>>
    {
        self.load_mapped_image(descriptor, None)
    }

    /// Load and cache DWARF for the slice of the file `slice` names (#175).
    ///
    /// The cache key includes the slice. When loading several slices at the same
    /// address, use the returned images directly for symbol lookups to specify which
    /// slice to query.
    ///
    /// # Errors
    /// Returns the same errors as [`load_image`](Self::load_image), including
    /// [`DebuggerError::NoDebugInfo`] if the file has no such slice. A file of another
    /// build than `slice` names returns [`DebuggerError::InvalidArgument`].
    pub fn load_image_slice(&mut self, descriptor: ImageDescriptor, slice: &ImageSlice) -> Result<Arc<BinaryImage>>
    {
        self.load_mapped_image(descriptor, Some(slice))
    }

    /// [`load_image`](Self::load_image), reading the slice `slice` names when it is
    /// known (#169).
    pub(crate) fn load_mapped_image(
        &mut self,
        descriptor: ImageDescriptor,
        slice: Option<&ImageSlice>,
    ) -> Result<Arc<BinaryImage>>
    {
        let canonical = if descriptor.path.is_absolute() {
            descriptor.path.clone()
        } else {
            descriptor.path.canonicalize().map_err(|err| {
                DebuggerError::InvalidArgument(format!("unable to canonicalize {}: {err}", descriptor.path.display()))
            })?
        };

        let id = ImageId::with_slice(&canonical, descriptor.load_address, slice);
        if let Some(existing) = self.images.get(&id) {
            return Ok(existing.clone());
        }

        let contents = self.format.open_image(&canonical, descriptor.load_address, slice)?;
        let image = Arc::new(BinaryImage::from_contents(
            ImageDescriptor {
                path: canonical,
                load_address: descriptor.load_address,
            },
            slice,
            contents,
        ));
        debug_assert_eq!(image.id(), id);
        self.images.insert(id, image.clone());
        Ok(image)
    }

    /// Stamp the files [`load_image`](Self::load_image) would read for `descriptor`,
    /// without reading them. A relative path that no longer resolves is stamped as given.
    pub(crate) fn load_inputs(&self, descriptor: &ImageDescriptor) -> LoadInputs
    {
        let path = if descriptor.path.is_absolute() {
            descriptor.path.clone()
        } else {
            descriptor.path.canonicalize().unwrap_or_else(|_| descriptor.path.clone())
        };
        LoadInputs::of(self.format.load_inputs(&path))
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
