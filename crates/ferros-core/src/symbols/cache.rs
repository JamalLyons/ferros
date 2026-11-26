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
use crate::types::{Address, BuildIdentity, FunctionParameter, SourceLocation, SymbolName, same_build};

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
    /// The build identity each image in `images` read from its file
    /// ([`ImageContents::build_id`](crate::objfile::ImageContents::build_id)), so a build
    /// id the loader reports later can be checked against the file that was opened.
    file_builds: HashMap<ImageId, BuildIdentity>,
    format: Arc<dyn ObjectFormat>,
}

/// A public load whose image file was read but whose debug information was not usable.
struct MissingDebug
{
    inputs: ImageLoadInputs,
    error: DebuggerError,
}

/// Stamps of the executable and of the files consulted for its companion debug information.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ImageLoadInputs
{
    pub(crate) executable: LoadInputs,
    pub(crate) companions: CompanionLoadInputs,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CompanionLoadInputs
{
    listed: LoadInputs,
    discovered: LoadInputs,
}

impl ImageLoadInputs
{
    pub(crate) fn with_discovered_companions(mut self, image: &BinaryImage) -> Self
    {
        self.companions.discovered = image.debug_load_inputs();
        self
    }
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
            file_builds: HashMap::new(),
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
        self.file_builds.retain(|id, _| ids.contains(id));
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
    /// same error is returned after a few `stat` calls (#236). So is a file for which
    /// [`ObjectFormat::open_image`](crate::objfile::ObjectFormat::open_image) itself
    /// reports [`DebuggerError::NoDebugInfo`] (#244). Any other error is not kept.
    fn load_mapped_image(&mut self, mut descriptor: ImageDescriptor, slice: Option<&ImageSlice>)
    -> Result<Arc<BinaryImage>>
    {
        descriptor.path = Self::canonical(&descriptor)?;
        let id = ImageId::with_slice(&descriptor.path, descriptor.load_address, slice);
        if let Some(existing) = self.images.get(&id).filter(|image| image.has_debug_info()) {
            return Ok(existing.clone());
        }
        let missing = self.missing_debug.get(&id);
        let inputs = self.load_inputs(&descriptor, missing.map(|missing| &missing.inputs));
        if let Some(missing) = missing.filter(|missing| missing.inputs == inputs) {
            return Err(missing.error.duplicate());
        }
        let image = self.read_image(descriptor, slice);
        let (image, build_id) = match image {
            Ok(image) => image,
            // The format found no debug information without loading the image (for
            // example it cannot tell which part of the file is mapped): kept like missing
            // DWARF, since rereading unchanged files gives the same answer (#244).
            Err(error @ DebuggerError::NoDebugInfo { .. }) => {
                let reported = error.duplicate();
                let mut inputs = inputs;
                inputs.companions.discovered = LoadInputs::default();
                self.missing_debug.insert(id, MissingDebug { inputs, error });
                return Err(reported);
            }
            Err(error) => {
                self.missing_debug.remove(&id);
                return Err(error);
            }
        };
        if let Some(error) = image.debug_error() {
            let error = error.duplicate();
            let reported = error.duplicate();
            let inputs = inputs.with_discovered_companions(&image);
            self.missing_debug.insert(id, MissingDebug { inputs, error });
            return Err(reported);
        }
        self.missing_debug.remove(&id);
        self.images.insert(id, image.clone());
        self.file_builds.insert(id, build_id);
        Ok(image)
    }

    /// Load and cache the image `descriptor` names (using `slice` when known), with or
    /// without debug information. When refreshing a cached image, keep the previous entry
    /// until its replacement's executable has been read successfully.
    ///
    /// `mapped` is the build identity the loader reports for the mapping. A file that
    /// records another build is refused even when `slice` could not carry that identity to
    /// the format, so a file opened again after a late build id contradicted it is never
    /// cached for the wrong build (#221).
    ///
    /// # Errors
    /// An error of [`ObjectFormat::open_image`]: the file cannot be read, does not parse,
    /// is another build, or the format cannot tell which part of it is mapped.
    /// [`DebuggerError::InvalidArgument`] when `mapped` is known and the file records
    /// another build or none.
    pub(crate) fn load_executable(
        &mut self,
        mut descriptor: ImageDescriptor,
        slice: Option<&ImageSlice>,
        mapped: BuildIdentity,
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
        let path = descriptor.path.clone();
        let (image, build_id) = self.read_image(descriptor, slice)?;
        if mapped.known().is_some() && !same_build(build_id, mapped) {
            return Err(DebuggerError::InvalidArgument(format!(
                "{} is another build ({build_id:?}) than the mapped image ({mapped:?})",
                path.display()
            )));
        }
        if let Some(previous) = replacing.filter(|&previous| previous != id) {
            self.images.remove(&previous);
            self.file_builds.remove(&previous);
        }
        self.images.insert(id, image.clone());
        self.file_builds.insert(id, build_id);
        Ok(image)
    }

    /// Retry the companion lookup against the executable already in the cache.
    pub(crate) fn retry_debug_info(&mut self, id: ImageId) -> Option<Arc<BinaryImage>>
    {
        let image = Arc::new(self.images.get(&id)?.retry_debug_info()?);
        self.images.insert(id, image.clone());
        Some(image)
    }

    /// The build identity the file of the cached image `id` records, or `None` when `id`
    /// is not cached.
    pub(crate) fn file_build_id(&self, id: ImageId) -> Option<BuildIdentity>
    {
        self.file_builds.get(&id).copied()
    }

    /// Read the image and the build identity its file records.
    fn read_image(
        &self,
        descriptor: ImageDescriptor,
        slice: Option<&ImageSlice>,
    ) -> Result<(Arc<BinaryImage>, BuildIdentity)>
    {
        let (contents, retry_debug) =
            self.format
                .open_image_with_debug_retry(&descriptor.path, descriptor.load_address, slice)?;
        let build_id = contents.build_id;
        Ok((
            Arc::new(BinaryImage::from_contents(descriptor, slice, contents, retry_debug)),
            build_id,
        ))
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
    /// without reading them. Include any paths discovered on the previous attempt.
    /// A relative path that no longer resolves is stamped as given.
    pub(crate) fn load_inputs(&self, descriptor: &ImageDescriptor, previous: Option<&ImageLoadInputs>) -> ImageLoadInputs
    {
        let path = Self::canonical(descriptor).unwrap_or_else(|_| descriptor.path.clone());
        let identity = self.format.file_identity_reader();
        ImageLoadInputs {
            executable: LoadInputs::with_identity([path.clone()], identity),
            companions: CompanionLoadInputs {
                listed: LoadInputs::with_identity(
                    self.format.load_inputs(&path).into_iter().filter(|input| *input != path),
                    identity,
                ),
                discovered: previous.map_or_else(LoadInputs::default, |inputs| inputs.companions.discovered.refresh()),
            },
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

#[cfg(test)]
mod tests
{
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::Arc;

    use super::SymbolCache;
    use crate::objfile::mock::MockObjectFormat;
    use crate::objfile::{
        FileIdentity, FileIdentityReader, ImageContents, ImageSlice, MappedImage, MappedSymbols, ObjectFormat, TargetMemory,
    };
    use crate::symbols::ImageDescriptor;
    use crate::types::Address;

    /// [`MockObjectFormat`] that reports one fixed file identity.
    struct Reporting(FileIdentityReader);

    impl ObjectFormat for Reporting
    {
        fn file_identity_reader(&self) -> Option<FileIdentityReader>
        {
            Some(self.0)
        }

        fn mapped_symbols(
            &self,
            memory: &mut dyn TargetMemory,
            load_address: Address,
        ) -> Result<Arc<dyn MappedSymbols>, String>
        {
            MockObjectFormat.mapped_symbols(memory, load_address)
        }

        fn map_image(&self, memory: &mut dyn TargetMemory, load_address: Address) -> MappedImage
        {
            MockObjectFormat.map_image(memory, load_address)
        }

        fn open_image(
            &self,
            path: &std::path::Path,
            load_address: u64,
            slice: Option<&ImageSlice>,
        ) -> crate::Result<ImageContents>
        {
            MockObjectFormat.open_image(path, load_address, slice)
        }
    }

    fn fixed(_: &std::fs::Metadata) -> Option<FileIdentity>
    {
        Some(FileIdentity::new(1, 2, 3, 4))
    }

    fn moved(_: &std::fs::Metadata) -> Option<FileIdentity>
    {
        Some(FileIdentity::new(1, 2, 9, 0))
    }

    #[test]
    fn cache_stamps_use_the_format_file_identity()
    {
        let path = std::env::temp_dir().join(format!("ferros-cache-identity-{}", std::process::id()));
        let bytes = b"same-size-body";
        let mut file = std::fs::File::create(&path).expect("create sample");
        file.write_all(bytes).expect("write sample");
        let modified = file.metadata().expect("sample metadata").modified().expect("sample mtime");
        let descriptor = ImageDescriptor {
            path: path.clone(),
            load_address: 0x1000,
        };

        let cache = SymbolCache::new(Arc::new(Reporting(fixed)));
        let first = cache.load_inputs(&descriptor, None);
        file.seek(SeekFrom::Start(0)).expect("rewind sample");
        file.write_all(bytes).expect("rewrite sample");
        file.set_modified(modified).expect("restore mtime");
        let again = cache.load_inputs(&descriptor, None);
        assert_eq!(first, again, "the same identity must not look like a change");

        let changed = SymbolCache::new(Arc::new(Reporting(moved))).load_inputs(&descriptor, None);
        assert_ne!(
            first, changed,
            "the cache ignored the format's file identity for a same-size, same-mtime rewrite"
        );
        let _ = std::fs::remove_file(&path);
    }
}
