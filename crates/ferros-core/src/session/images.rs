//! Live image mappings and on-demand symbols for the portable session.

use std::path::PathBuf;

use super::Session;
use crate::backend::{BackendError, LoadedImage};
use crate::error::{DebuggerError, Result};
use crate::symbols::{DebugInfoOptions, ImageDescriptor, ImageMapping, SymbolCache};
use crate::types::Address;

pub(super) struct SessionImage
{
    descriptor: ImageDescriptor,
    /// The layout read from the image's mapped header, or why it could not be read.
    /// Mapped memory does not change while the image stays loaded at this address, so
    /// the result is kept until the loader reports it unloaded or moved.
    mapping: std::result::Result<ImageMapping, String>,
    attempted: bool,
    no_debug_info: Option<String>,
}

impl Session
{
    /// Configure companion discovery and clear any previous symbol-loading results.
    /// The options persist across launches; live mappings refresh on the next lookup.
    #[must_use]
    pub fn with_debug_info_options(mut self, options: DebugInfoOptions) -> Self
    {
        self.symbols = SymbolCache::with_debug_info_options(options);
        self.loaded_images.clear();
        self
    }

    /// The main executable and loaded libraries with their loader-reported addresses.
    ///
    /// # Errors
    /// Returns a state error unless stopped, or the backend's enumeration error.
    pub fn images(&mut self) -> Result<Vec<LoadedImage>>
    {
        self.require_stopped()?;
        Ok(self.backend.images()?)
    }

    /// Map a runtime address to the owning executable path and file virtual address.
    /// Segment gaps and unmapped images return `None`.
    ///
    /// # Errors
    /// Returns a state error unless stopped, or the backend's enumeration error.
    pub fn file_address(&mut self, address: Address) -> Result<Option<(PathBuf, u64)>>
    {
        self.require_stopped()?;
        self.load_images()?;
        Ok(self.loaded_images.iter().find_map(|image| {
            image
                .mapping
                .as_ref()
                .ok()?
                .file_address(address)
                .map(|file| (image.descriptor.path.clone(), file))
        }))
    }

    /// Map a file virtual address within a particular loaded image to runtime memory.
    /// `image` must still be loaded at the supplied load address; otherwise, or when the
    /// address is outside the image's mapped segments, the result is `None`.
    ///
    /// The layout comes from the Mach-O header the loader mapped, not from the file at
    /// `image.path`, so a file replaced or removed on disk does not change the result.
    ///
    /// # Errors
    /// Returns a state error unless stopped, the backend's enumeration error, or
    /// [`DebuggerError::ImageMapping`] with the original failure when the image's mapped
    /// header could not be read or parsed.
    pub fn runtime_address(&mut self, image: &LoadedImage, file_address: u64) -> Result<Option<Address>>
    {
        self.require_stopped()?;
        self.load_images()?;
        let Some(entry) = self.loaded_images.iter().find(|entry| {
            entry.descriptor.path == image.path && entry.descriptor.load_address == image.load_address.value()
        }) else {
            return Ok(None);
        };
        match &entry.mapping {
            Ok(mapping) => Ok(mapping.runtime_address(file_address)),
            Err(detail) => Err(DebuggerError::ImageMapping {
                path: entry.descriptor.path.clone(),
                detail: detail.clone(),
            }),
        }
    }

    /// Refresh mappings without requesting DWARF or reading image files. Images whose
    /// mapped header has no segment at file offset zero (for example libraries in the
    /// shared cache) stay unmapped, which is harmless here.
    pub(super) fn load_images(&mut self) -> Result<()>
    {
        let images = match self.backend.images() {
            Ok(images) => images,
            Err(BackendError::Unsupported(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let descriptors: Vec<_> = images
            .into_iter()
            .map(|image| ImageDescriptor {
                path: image.path,
                load_address: image.load_address.value(),
            })
            .collect();
        self.symbols.retain_images(&descriptors);
        let mut previous = std::mem::take(&mut self.loaded_images);
        for descriptor in descriptors {
            if let Some(index) = previous.iter().position(|entry| {
                entry.descriptor.path == descriptor.path && entry.descriptor.load_address == descriptor.load_address
            }) {
                self.loaded_images.push(previous.swap_remove(index));
            } else {
                let mapping = self.map_loaded_image(descriptor.load_address);
                self.loaded_images.push(SessionImage {
                    descriptor,
                    mapping,
                    attempted: false,
                    no_debug_info: None,
                });
            }
        }
        Ok(())
    }

    /// Read the segment layout from the header mapped at `load_address`. Trap bytes are
    /// irrelevant: breakpoints are never planted in a Mach-O header.
    fn map_loaded_image(&mut self, load_address: u64) -> std::result::Result<ImageMapping, String>
    {
        let address = Address::new(load_address);
        let header = self
            .backend
            .read_memory(address, ImageMapping::MACH_HEADER_SIZE)
            .map_err(|error| format!("reading the Mach-O header at {address}: {error}"))?;
        let size = ImageMapping::header_size(&header).map_err(|error| format!("header at {address}: {error}"))?;
        let header = self
            .backend
            .read_memory(address, size)
            .map_err(|error| format!("reading {size} bytes of load commands at {address}: {error}"))?;
        ImageMapping::from_header(&header, load_address).map_err(|error| format!("header at {address}: {error}"))
    }

    /// Load only the image needed for a lookup. Cache a typed failure so the diagnostic
    /// survives subsequent requests without repeatedly running dsymutil.
    pub(super) fn load_symbols_for_address(&mut self, address: Address) -> Result<bool>
    {
        let Some(image) = self
            .loaded_images
            .iter_mut()
            .find(|image| image.mapping.as_ref().is_ok_and(|mapping| mapping.contains(address)))
        else {
            return Ok(false);
        };
        if let Some(detail) = &image.no_debug_info {
            return Err(DebuggerError::NoDebugInfo {
                path: image.descriptor.path.clone(),
                detail: detail.clone(),
            });
        }
        if image.attempted {
            return Ok(false);
        }
        image.attempted = true;
        match self.symbols.load_image(image.descriptor.clone()) {
            Ok(_) => Ok(true),
            Err(DebuggerError::NoDebugInfo { detail, path }) => {
                image.no_debug_info = Some(detail.clone());
                Err(DebuggerError::NoDebugInfo { path, detail })
            }
            Err(error) => {
                tracing::debug!("could not load symbols for {}: {error}", image.descriptor.path.display());
                Ok(false)
            }
        }
    }
}
