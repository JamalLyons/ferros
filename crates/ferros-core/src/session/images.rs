//! Live image mappings and on-demand symbols for the portable session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::Session;
use super::stack::BackendTarget;
use crate::backend::{BackendError, LoadedImage};
use crate::error::{DebuggerError, Result};
use crate::objfile::{ImageSlice, MappedImage, MappedSymbols, ObjectFormat, SymbolIndex, SymbolKey};
use crate::symbols::load_inputs::LoadInputs;
use crate::symbols::{ImageDescriptor, ImageId, ImageMapping, SymbolCache};
use crate::types::{Address, BuildIdentity, same_build};

/// The image's file, or only its source debug information, could not be loaded (see
/// [`SessionImage`]).
///
/// Failures other than missing debug info are kept with a stamp of the files the load
/// read, taken just before it read them. Later requests compare a fresh stamp (a few
/// `stat` calls) and, while it is equal, return `error` again without reading or parsing
/// the image. A file that appears, disappears or changes makes the next request load
/// the image again.
struct FailedLoad
{
    /// The symbol request in which `error` was last reported, if any. Within one request
    /// the failure is reported once.
    reported: Option<u64>,
    /// The symbol request in which `inputs` was last compared with the files. Within one
    /// request the image is not read again.
    checked: u64,
    inputs: LoadInputs,
    error: DebuggerError,
}

/// A loaded image and what the session learned about it while that same image stays
/// mapped: its mapping, its symbols, and why they could not be loaded.
///
/// The same image is the same path and load address with build identities that can be
/// the same build ([`same_build`]). Two known, different ids, or an id the image format
/// does not record ([`BuildIdentity::Absent`]), mean the image may have been replaced;
/// then the mapping, the symbol table, the DWARF id and the failure records are dropped
/// and read again. An unknown id ([`BuildIdentity::Unknown`]) on either side is no
/// evidence of a change: a read can fail at one stop and succeed at the next. The session
/// also records the id from the mapped header when the backend could not read it (#170).
///
/// Symbol loading keeps one of three outcomes: the loaded symbols; missing debug info
/// (kept until the image is unloaded); or another failure, kept only while the files it
/// read are unchanged ([`FailedLoad`]). So a persistent failure, such as a corrupt image
/// file or a malformed companion debug file, costs one read and parse, not one per request, and a
/// transient one, such as a file Cargo is replacing, recovers once the file is back.
pub(super) struct SessionImage
{
    descriptor: ImageDescriptor,
    /// Build identity reported by the backend or read from the mapped header. An unknown
    /// id is replaced once a later report knows it.
    build_id: BuildIdentity,
    /// The session's stop generation when this image was read. An image with an
    /// [`BuildIdentity::Absent`] id is kept only within that stop ([`Self::same_image`]).
    generation: u64,
    /// The layout read from the image's mapped header, or why it could not be read.
    /// Mapped memory does not change while this image stays loaded, so the result is kept
    /// until the loader reports a different path, address or build ([`same_build`]).
    mapping: std::result::Result<ImageMapping, String>,
    /// Which slice and build of the image's file is mapped, read with `mapping` by the
    /// session's [`ObjectFormat`]: the part of a file holding several images the symbols
    /// are loaded from (#169). `None` when the header could not be read.
    slice: Option<ImageSlice>,
    /// The [`SymbolCache`] entry holding this image's unwind tables and, unless
    /// `no_debug_info` or `failed_load` says why not, its source debug information,
    /// once loaded. The cache keys
    /// images by canonical path, load address and the slice known at parse time
    /// ([`ImageId`]). Retention uses this id because a relative loader path may stop
    /// resolving later (#99).
    symbols: Option<ImageId>,
    /// Why the image has no usable DWARF. Kept so the diagnostic survives later requests
    /// without repeatedly running the format's debug-info discovery. With `symbols` set
    /// the image still unwinds from its own unwind tables.
    no_debug_info: Option<String>,
    /// Why the image's last load failed for another reason, and what it read: the whole
    /// image when `symbols` is unset, only its debug information when it is set.
    failed_load: Option<FailedLoad>,
    /// The image's symbol table layout as mapped in the target, read on the first frame
    /// that needs it (see [`Session::symbol_table_name`](super::Session)). Like `mapping`,
    /// it holds until the loader reports a different path, address or build ([`same_build`]).
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
    /// Where the table and the image's code lie, from the session's
    /// [`ObjectFormat`](crate::objfile::ObjectFormat).
    pub(super) symbols: Arc<dyn MappedSymbols>,
    /// Unset until a pc in the image's code needs a name.
    pub(super) index: Option<Box<dyn SymbolIndex>>,
    /// The symbol request whose read of the table failed.
    pub(super) index_failed_in_request: Option<u64>,
    /// Names read from the target, by symbol; `None` for one that is empty or not UTF-8.
    pub(super) names: HashMap<SymbolKey, Option<String>>,
}

impl SessionSymtab
{
    pub(super) fn new(symbols: Arc<dyn MappedSymbols>) -> Self
    {
        Self {
            symbols,
            index: None,
            index_failed_in_request: None,
            names: HashMap::new(),
        }
    }
}

/// Whether a cached image read at stop `generation` with id `cached` is the image now
/// reported with id `reported` at the same path and load address, at stop `current`.
///
/// Two ids that can be the same build ([`same_build`]) are. An id the format does not record
/// ([`BuildIdentity::Absent`]) is no evidence across stops, but within one stop the
/// process has not run and cannot have replaced the image, so it is reused then: an
/// `Absent` image costs one read per stop, not one per request.
pub(super) fn same_image(cached: BuildIdentity, generation: u64, reported: BuildIdentity, current: u64) -> bool
{
    same_build(cached, reported) || (generation == current && cached == reported)
}

impl SessionImage
{
    pub(super) fn descriptor_load_address(&self) -> u64
    {
        self.descriptor.load_address
    }

    pub(super) fn descriptor_path(&self) -> &std::path::Path
    {
        &self.descriptor.path
    }

    pub(super) fn build_id(&self) -> BuildIdentity
    {
        self.build_id
    }
}

impl Session
{
    /// Read images as `format` from now on, clearing any previous mapping and
    /// symbol-loading results. A platform crate configures how its format finds companion
    /// debug information this way. The format persists across launches; live mappings
    /// refresh on the next lookup.
    #[must_use]
    pub fn with_object_format(mut self, format: Arc<dyn ObjectFormat>) -> Self
    {
        self.symbols = SymbolCache::new(format.clone());
        self.format = format;
        self.loaded_images.clear();
        self.exit_images.clear();
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
    /// `image` must still be loaded at the supplied load address as the same build
    /// ([`same_build`]); otherwise, or when the address is outside the image's
    /// mapped segments, the result is `None`.
    ///
    /// The layout comes from the image header the loader mapped, not from the file at
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
            entry.descriptor.path == image.path
                && entry.descriptor.load_address == image.load_address.value()
                && same_image(entry.build_id, entry.generation, image.build_id, self.stop_generation)
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
    /// mapped header has no segment at file offset zero (for example system libraries the
    /// loader maps from a shared region) stay unmapped, which is harmless here.
    ///
    /// A previous [`SessionImage`] is reused only when its path and load address are
    /// unchanged and its build identity can be the same build ([`same_build`]). Otherwise a
    /// fresh image starts, and its DWARF cache id is then absent from the set passed to
    /// [`SymbolCache::retain_ids`].
    pub(super) fn load_images(&mut self) -> Result<()>
    {
        let images = match self.backend.images() {
            Ok(images) => images,
            Err(BackendError::Unsupported(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut previous = std::mem::take(&mut self.loaded_images);
        for image in images {
            let descriptor = ImageDescriptor {
                path: image.path,
                load_address: image.load_address.value(),
            };
            if let Some(index) = previous.iter().position(|entry| {
                entry.descriptor.path == descriptor.path
                    && entry.descriptor.load_address == descriptor.load_address
                    && same_image(entry.build_id, entry.generation, image.build_id, self.stop_generation)
            }) {
                let mut kept = previous.swap_remove(index);
                kept.build_id = kept.build_id.or_later(image.build_id);
                kept.slice = kept.slice.map(|slice| slice.or_later(kept.build_id));
                self.loaded_images.push(kept);
            } else {
                let MappedImage { mapping, slice } = self.map_loaded_image(descriptor.load_address, image.build_id);
                self.loaded_images.push(SessionImage {
                    descriptor,
                    generation: self.stop_generation,
                    build_id: image
                        .build_id
                        .or_later(slice.as_ref().map_or(BuildIdentity::Unknown, ImageSlice::build_id)),
                    mapping,
                    slice,
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

    /// Read the segment layout and the slice from the header mapped at `load_address`,
    /// through the session's [`ObjectFormat`]. Trap bytes are irrelevant: breakpoints are
    /// never planted in an image header. The header's build id is the slice's; the
    /// loader-reported `build_id` stands in only when the header's is unknown.
    fn map_loaded_image(&mut self, load_address: u64, build_id: BuildIdentity) -> MappedImage
    {
        let MappedImage { mapping, slice } = self
            .format
            .map_image(&mut BackendTarget(self.backend.as_mut()), Address::new(load_address));
        MappedImage {
            mapping,
            slice: slice.map(|slice| slice.or_later(build_id)),
        }
    }

    /// Start a request that may load symbols. Within one request an image whose load
    /// failed is not read again; the next request retries it.
    pub(super) fn begin_symbol_request(&mut self)
    {
        self.symbol_request = self.symbol_request.wrapping_add(1);
    }

    /// Load only the image needed for a lookup, with its source debug information.
    /// Returns whether the image was newly loaded; `Ok(false)` when no mapped image
    /// contains `address`, it is already loaded, or its failure was already reported in
    /// the current request.
    ///
    /// # Errors
    /// [`DebuggerError::NoDebugInfo`] when the image has no usable DWARF (kept while the
    /// image stays loaded), or [`DebuggerError::SymbolLoad`] with the original failure
    /// for any other load error (kept while the files the load read are unchanged; see
    /// [`SessionImage`]). An image whose file could be read keeps its unwind tables
    /// loaded either way ([`load_executable_for_address`](Self::load_executable_for_address)).
    pub(super) fn load_symbols_for_address(&mut self, address: Address) -> Result<bool>
    {
        let request = self.symbol_request;
        let loaded = self.load_executable_for_address(address)?;
        let Some(image) = self.image_containing(address) else {
            return Ok(loaded);
        };
        if image.symbols.is_none() {
            return Ok(loaded);
        }
        if let Some(detail) = &image.no_debug_info {
            return Err(DebuggerError::NoDebugInfo {
                path: image.descriptor.path.clone(),
                detail: detail.clone(),
            });
        }
        match &mut image.failed_load {
            Some(failed) if failed.reported != Some(request) => {
                failed.reported = Some(request);
                Err(DebuggerError::SymbolLoad {
                    path: image.descriptor.path.clone(),
                    source: Box::new(failed.error.duplicate()),
                })
            }
            _ => Ok(loaded),
        }
    }

    /// Load the image containing `address` into the [`SymbolCache`] for its layout and
    /// unwind tables, and record the outcome of its source debug information on the
    /// [`SessionImage`] without failing on it. Returns whether the image was newly
    /// loaded: a stack trace walks again then, since the image's unwind tables can reveal
    /// callers that the frame-pointer chain missed.
    ///
    /// Missing debug information is kept while the image stays loaded. Debug information
    /// that failed for another reason is read again once the files the load read change,
    /// compared at most once per request.
    ///
    /// # Errors
    /// Only for an image whose file could not be loaded at all, once per request:
    /// [`DebuggerError::NoDebugInfo`] when the format cannot tell which part of the file is
    /// mapped, [`DebuggerError::SymbolLoad`] for any other failure.
    pub(super) fn load_executable_for_address(&mut self, address: Address) -> Result<bool>
    {
        let request = self.symbol_request;
        let Some(image) = self
            .loaded_images
            .iter_mut()
            .find(|image| image.mapping.as_ref().is_ok_and(|mapping| mapping.contains(address)))
        else {
            return Ok(false);
        };
        if image.symbols.is_none()
            && let Some(detail) = &image.no_debug_info
        {
            return Err(DebuggerError::NoDebugInfo {
                path: image.descriptor.path.clone(),
                detail: detail.clone(),
            });
        }
        let inputs = match &mut image.failed_load {
            None if image.symbols.is_some() => return Ok(false),
            None => self.symbols.load_inputs(&image.descriptor),
            Some(failed) => {
                if failed.checked == request {
                    return Ok(false);
                }
                failed.checked = request;
                let inputs = self.symbols.load_inputs(&image.descriptor);
                if failed.inputs == inputs {
                    if image.symbols.is_some() {
                        // The debug information failed; `load_symbols_for_address` reports it.
                        return Ok(false);
                    }
                    failed.reported = Some(request);
                    return Err(DebuggerError::SymbolLoad {
                        path: image.descriptor.path.clone(),
                        source: Box::new(failed.error.duplicate()),
                    });
                }
                inputs
            }
        };
        // Never loaded, or loaded with failed debug information whose files changed since.
        if let Some(id) = image.symbols.take() {
            self.symbols.forget(id);
        }
        image.failed_load = None;
        match self.symbols.load_executable(image.descriptor.clone(), image.slice.as_ref()) {
            Ok(loaded) => {
                image.symbols = Some(loaded.id());
                match loaded.debug_error() {
                    None => {}
                    Some(DebuggerError::NoDebugInfo { detail, .. }) => image.no_debug_info = Some(detail.clone()),
                    Some(error) => {
                        image.failed_load = Some(FailedLoad {
                            reported: None,
                            checked: request,
                            inputs,
                            error: error.duplicate(),
                        });
                    }
                }
                Ok(true)
            }
            Err(DebuggerError::NoDebugInfo { detail, path }) => {
                image.no_debug_info = Some(detail.clone());
                Err(DebuggerError::NoDebugInfo { path, detail })
            }
            Err(error) => {
                let source = Box::new(error.duplicate());
                image.failed_load = Some(FailedLoad {
                    reported: Some(request),
                    checked: request,
                    inputs,
                    error,
                });
                Err(DebuggerError::SymbolLoad {
                    path: image.descriptor.path.clone(),
                    source,
                })
            }
        }
    }

    fn image_containing(&mut self, address: Address) -> Option<&mut SessionImage>
    {
        self.loaded_images
            .iter_mut()
            .find(|image| image.mapping.as_ref().is_ok_and(|mapping| mapping.contains(address)))
    }
}
