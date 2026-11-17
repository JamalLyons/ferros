//! Live image mappings and on-demand symbols for the portable session.

use std::collections::HashMap;
use std::path::PathBuf;

use super::Session;
use crate::backend::{BackendError, LoadedImage};
use crate::error::{DebuggerError, Result};
use crate::symbols::debuginfo::LoadInputs;
use crate::symbols::symtab::{SymbolTable, SymtabLayout};
use crate::symbols::{DebugInfoOptions, ImageDescriptor, ImageId, ImageMapping, SymbolCache};
use crate::types::Address;

/// The image's symbols could not be loaded (see [`SessionImage`]).
///
/// Failures other than missing debug info are kept with a stamp of the files the load
/// read, taken just before it read them. Later requests compare a fresh stamp (a few
/// `stat` calls) and, while it is equal, return `error` again without reading or parsing
/// the image. A file that appears, disappears or changes makes the next request load
/// the image again.
struct FailedLoad
{
    /// The symbol request in which `error` was last reported. Within one request the
    /// failure is reported once and the image is not read again.
    request: u64,
    inputs: LoadInputs,
    error: DebuggerError,
}

/// A loaded image and what the session learned about it while it stays mapped at its
/// load address: its mapping, its symbols, and why they could not be loaded.
///
/// Symbol loading keeps one of three outcomes: the loaded symbols; missing debug info
/// (kept until the image is unloaded); or another failure, kept only while the files it
/// read are unchanged ([`FailedLoad`]). So a persistent failure, such as a corrupt image
/// file or a malformed dSYM, costs one read and parse, not one per request, and a
/// transient one, such as a file Cargo is replacing, recovers once the file is back.
pub(super) struct SessionImage
{
    descriptor: ImageDescriptor,
    /// The layout read from the image's mapped header, or why it could not be read.
    /// Mapped memory does not change while the image stays loaded at this address, so
    /// the result is kept until the loader reports it unloaded or moved.
    mapping: std::result::Result<ImageMapping, String>,
    /// The [`SymbolCache`] entry holding this image's symbols, once loaded. The cache keys
    /// images by the canonical path resolved at load time; a relative loader path may stop
    /// resolving later (issue #99), so retention follows this id, not the path.
    symbols: Option<ImageId>,
    /// Why the image has no usable DWARF. Kept so the diagnostic survives later requests
    /// without repeatedly running dsymutil.
    no_debug_info: Option<String>,
    /// Why the image's last symbol load failed for another reason, and what it read.
    failed_load: Option<FailedLoad>,
    /// The image's symbol table layout as mapped in the target, read on the first frame
    /// that needs it (see [`Session::symbol_table_name`](super::Session)). Like `mapping`,
    /// it holds until the image is unloaded or moved.
    pub(super) symtab: Option<SessionSymtab>,
    /// The symbol request whose read of the symbol table layout last failed. The failure
    /// is not kept past that request.
    pub(super) symtab_failed_in_request: Option<u64>,
}

/// An image's symbol table and the names read from it so far.
///
/// Only what was read is kept. A failed read is retried by the next symbol request, not
/// within the one where it failed (`table`), or on the next lookup (names).
pub(super) struct SessionSymtab
{
    pub(super) layout: SymtabLayout,
    /// Unset until a pc in the image's code needs a name.
    pub(super) table: Option<SymbolTable>,
    /// The symbol request whose read of the `nlist` entries or function starts failed.
    pub(super) table_failed_in_request: Option<u64>,
    /// Names read from the target, by string table index; `None` for one that is empty or
    /// not UTF-8.
    pub(super) names: HashMap<u32, Option<String>>,
}

impl SessionSymtab
{
    pub(super) fn new(layout: SymtabLayout) -> Self
    {
        Self {
            layout,
            table: None,
            table_failed_in_request: None,
            names: HashMap::new(),
        }
    }
}

impl SessionImage
{
    pub(super) fn descriptor_load_address(&self) -> u64
    {
        self.descriptor.load_address
    }
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

    /// Load address of the main executable (the first image the backend reports), after
    /// [`load_images`](Self::load_images). `None` when the backend reports no image.
    ///
    /// # Errors
    /// [`DebuggerError::ImageMapping`] when the main image's mapped header could not be read.
    pub(super) fn main_load_address(&self) -> Result<Option<Address>>
    {
        let Some(main) = self.loaded_images.first() else {
            return Ok(None);
        };
        match &main.mapping {
            Ok(_) => Ok(Some(Address::new(main.descriptor.load_address))),
            Err(detail) => Err(DebuggerError::ImageMapping {
                path: main.descriptor.path.clone(),
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
                    symbols: None,
                    no_debug_info: None,
                    failed_load: None,
                    symtab: None,
                    symtab_failed_in_request: None,
                });
            }
        }
        self.symbols
            .retain_ids(&self.loaded_images.iter().filter_map(|image| image.symbols).collect());
        Ok(())
    }

    /// Read the segment layout from the header mapped at `load_address`. Trap bytes are
    /// irrelevant: breakpoints are never planted in a Mach-O header.
    fn map_loaded_image(&mut self, load_address: u64) -> std::result::Result<ImageMapping, String>
    {
        let header = self.read_mapped_header(load_address)?;
        ImageMapping::from_header(&header, load_address)
            .map_err(|error| format!("header at {}: {error}", Address::new(load_address)))
    }

    /// The Mach-O header and load commands mapped at `load_address`.
    pub(super) fn read_mapped_header(&mut self, load_address: u64) -> std::result::Result<Vec<u8>, String>
    {
        let address = Address::new(load_address);
        let header = self
            .backend
            .read_memory(address, ImageMapping::MACH_HEADER_SIZE)
            .map_err(|error| format!("reading the Mach-O header at {address}: {error}"))?;
        let size = ImageMapping::header_size(&header).map_err(|error| format!("header at {address}: {error}"))?;
        self.backend
            .read_memory(address, size)
            .map_err(|error| format!("reading {size} bytes of load commands at {address}: {error}"))
    }

    /// Start a request that may load symbols. Within one request an image whose load
    /// failed is not read again; the next request retries it.
    pub(super) fn begin_symbol_request(&mut self)
    {
        self.symbol_request = self.symbol_request.wrapping_add(1);
    }

    /// Load only the image needed for a lookup. Returns whether symbols were newly
    /// loaded; `Ok(false)` when no mapped image contains `address`, its symbols are
    /// already loaded, or its load already failed in the current request.
    ///
    /// # Errors
    /// [`DebuggerError::NoDebugInfo`] when the image has no usable DWARF (kept while the
    /// image stays loaded), or [`DebuggerError::SymbolLoad`] with the original failure
    /// for any other load error (kept while the files the load read are unchanged; see
    /// [`SessionImage`]).
    pub(super) fn load_symbols_for_address(&mut self, address: Address) -> Result<bool>
    {
        let request = self.symbol_request;
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
        if image.symbols.is_some() {
            return Ok(false);
        }
        let inputs = self.symbols.load_inputs(&image.descriptor);
        if let Some(failed) = &mut image.failed_load {
            if failed.request == request {
                return Ok(false);
            }
            if failed.inputs == inputs {
                failed.request = request;
                return Err(DebuggerError::SymbolLoad {
                    path: image.descriptor.path.clone(),
                    source: Box::new(failed.error.duplicate()),
                });
            }
        }
        match self.symbols.load_image(image.descriptor.clone()) {
            Ok(loaded) => {
                image.symbols = Some(loaded.id());
                image.failed_load = None;
                Ok(true)
            }
            Err(DebuggerError::NoDebugInfo { detail, path }) => {
                image.failed_load = None;
                image.no_debug_info = Some(detail.clone());
                Err(DebuggerError::NoDebugInfo { path, detail })
            }
            Err(error) => {
                let source = Box::new(error.duplicate());
                image.failed_load = Some(FailedLoad { request, inputs, error });
                Err(DebuggerError::SymbolLoad {
                    path: image.descriptor.path.clone(),
                    source,
                })
            }
        }
    }
}
