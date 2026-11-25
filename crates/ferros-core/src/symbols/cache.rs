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
    /// Why a public load found no usable debug information, per image, with a stamp of
    /// the files it read taken just before reading them. While the stamp is unchanged a
    /// repeated load returns the error again without reading the file. Kept apart from
    /// `images`: a failed public load does not make the image visible to lookups.
    missing_debug: HashMap<ImageId, MissingDebug>,
    format: Arc<dyn ObjectFormat>,
}

/// A public load whose image file was read but whose debug information was not usable.
struct MissingDebug
{
    inputs: ImageLoadInputs,
    error: DebuggerError,
}

/// Stamps of the executable and of the files consulted for its companion debug information.
#[derive(PartialEq, Eq)]
pub(crate) struct ImageLoadInputs
{
    pub(crate) executable: LoadInputs,
    pub(crate) companions: LoadInputs,
}

impl SymbolCache
{
    /// Create an empty symbol cache reading image files as `format`.
    #[must_use]
    pub fn new(format: Arc<dyn ObjectFormat>) -> Self
    {
        Self {
            images: HashMap::new(),
            missing_debug: HashMap::new(),
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
        self.missing_debug.retain(|id, _| ids.contains(id));
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
    /// known (#169). Only a successful DWARF load changes the images lookups see.
    ///
    /// A file that was read but has no usable debug information is not read again while
    /// the files the load read are unchanged ([`load_inputs`](Self::load_inputs)): the
    /// same error is returned after a few `stat` calls (#236).
    fn load_mapped_image(&mut self, mut descriptor: ImageDescriptor, slice: Option<&ImageSlice>)
    -> Result<Arc<BinaryImage>>
    {
        descriptor.path = Self::canonical(&descriptor)?;
        let id = ImageId::with_slice(&descriptor.path, descriptor.load_address, slice);
        if let Some(existing) = self.images.get(&id).filter(|image| image.has_debug_info()) {
            return Ok(existing.clone());
        }
        let inputs = self.load_inputs(&descriptor);
        if let Some(missing) = self.missing_debug.get(&id).filter(|missing| missing.inputs == inputs) {
            return Err(missing.error.duplicate());
        }
        let image = self.read_image(descriptor, slice);
        let image = match image {
            Ok(image) => image,
            Err(error) => {
                self.missing_debug.remove(&id);
                return Err(error);
            }
        };
        if let Some(error) = image.debug_error() {
            let error = error.duplicate();
            let reported = error.duplicate();
            self.missing_debug.insert(id, MissingDebug { inputs, error });
            return Err(reported);
        }
        self.missing_debug.remove(&id);
        self.images.insert(id, image.clone());
        Ok(image)
    }

    /// Load and cache the image `descriptor` names (using `slice` when known), with or
    /// without debug information. When refreshing a cached image, keep the previous entry
    /// until its replacement's executable has been read successfully.
    ///
    /// # Errors
    /// An error of [`ObjectFormat::open_image`]: the file cannot be read, does not parse,
    /// is another build, or the format cannot tell which part of it is mapped.
    pub(crate) fn load_executable(
        &mut self,
        mut descriptor: ImageDescriptor,
        slice: Option<&ImageSlice>,
        replacing: Option<ImageId>,
    ) -> Result<Arc<BinaryImage>>
    {
        descriptor.path = Self::canonical(&descriptor)?;
        let id = ImageId::with_slice(&descriptor.path, descriptor.load_address, slice);
        if replacing.is_none()
            && let Some(existing) = self.images.get(&id)
        {
            return Ok(existing.clone());
        }
        let image = self.read_image(descriptor, slice)?;
        if let Some(previous) = replacing.filter(|&previous| previous != id) {
            self.images.remove(&previous);
        }
        self.images.insert(id, image.clone());
        Ok(image)
    }

    /// Retry the companion lookup against the executable already in the cache.
    pub(crate) fn retry_debug_info(&mut self, id: ImageId) -> Option<Arc<BinaryImage>>
    {
        let image = Arc::new(self.images.get(&id)?.retry_debug_info()?);
        self.images.insert(id, image.clone());
        Some(image)
    }

    fn read_image(&self, descriptor: ImageDescriptor, slice: Option<&ImageSlice>) -> Result<Arc<BinaryImage>>
    {
        let (contents, retry_debug) =
            self.format
                .open_image_with_debug_retry(&descriptor.path, descriptor.load_address, slice)?;
        Ok(Arc::new(BinaryImage::from_contents(descriptor, slice, contents, retry_debug)))
    }

    fn canonical(descriptor: &ImageDescriptor) -> Result<std::path::PathBuf>
    {
        if descriptor.path.is_absolute() {
            Ok(descriptor.path.clone())
        } else {
            descriptor.path.canonicalize().map_err(|err| {
                DebuggerError::InvalidArgument(format!("unable to canonicalize {}: {err}", descriptor.path.display()))
            })
        }
    }

    /// Stamp the files [`load_image`](Self::load_image) would read for `descriptor`,
    /// without reading them. A relative path that no longer resolves is stamped as given.
    pub(crate) fn load_inputs(&self, descriptor: &ImageDescriptor) -> ImageLoadInputs
    {
        let path = Self::canonical(descriptor).unwrap_or_else(|_| descriptor.path.clone());
        ImageLoadInputs {
            executable: LoadInputs::of([path.clone()]),
            companions: LoadInputs::of(self.format.load_inputs(&path).into_iter().filter(|input| *input != path)),
        }
    }

    /// Find the binary image containing the given address.
    ///
    /// Searches through all cached images to find one that contains the address
    /// (i.e., the address is within the image's load address range). The image may have
    /// no source debug information ([`BinaryImage::has_debug_info`]) when it was loaded
    /// only for its unwind tables.
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
        // An image kept only for unwinding has no types to describe.
        for image in self.images.values().filter(|image| image.has_debug_info()) {
            if let Some(summary) = image.describe_type(name)? {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }
}
