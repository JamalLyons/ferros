//! Live image mappings and on-demand symbols for the portable session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::Session;
use super::stack::BackendTarget;
use crate::backend::{BackendError, ImageRole, LoadedImage};
use crate::error::{DebuggerError, Result};
use crate::objfile::{ImageSlice, MappedImage, MappedSymbols, ObjectFormat, SymbolIndex, SymbolKey};
use crate::symbols::cache::ImageLoadInputs;
use crate::symbols::{ImageDescriptor, ImageId, ImageMapping, SymbolCache};
use crate::types::{Address, BuildIdentity, same_build};

/// The image's file, or only its source debug information, could not be loaded (see
/// [`SessionImage`]).
///
/// Failures are kept with a stamp of the files the load read, taken just before it
/// read them. Later requests compare a fresh stamp and, while it is equal, return
/// `error` again. A file that appears, disappears or changes makes the next request
/// retry. Learning the mapped build id also invalidates a failure to open the image.
struct FailedLoad
{
    /// The symbol request in which `error` was last reported, if any. Within one request
    /// the failure is reported once.
    reported: Option<u64>,
    /// The symbol request in which `inputs` was last compared with the files. Within one
    /// request the image is not read again.
    checked: u64,
    inputs: ImageLoadInputs,
    error: DebuggerError,
}

impl FailedLoad
{
    fn report(&mut self, request: u64, path: &Path) -> Result<()>
    {
        if self.reported == Some(request) {
            return Ok(());
        }
        self.reported = Some(request);
        Err(match &self.error {
            DebuggerError::NoDebugInfo { detail, .. } => DebuggerError::NoDebugInfo {
                path: path.to_path_buf(),
                detail: detail.clone(),
            },
            error => DebuggerError::SymbolLoad {
                path: path.to_path_buf(),
                source: Box::new(error.duplicate()),
            },
        })
    }
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
/// A load failure is kept while its inputs are unchanged ([`FailedLoad`]). An image
/// whose executable was read keeps its unwind information even when source discovery
/// fails. A change to the companion inputs can repair the source information over the
/// same cached executable.
pub(super) struct SessionImage
{
    descriptor: ImageDescriptor,
    /// Whether the backend marked this image as the main executable at the last refresh.
    role: ImageRole,
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
    /// `failed_load` says why not, its source debug information, once loaded. The cache keys
    /// images by canonical path, load address and the slice known at parse time
    /// ([`ImageId`]). Retention uses this id because a relative loader path may stop
    /// resolving later (#99).
    symbols: Option<ImageId>,
    /// Why the image or its source debug information could not be read, and its inputs.
    /// With `symbols` set, the cache still holds a usable executable.
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
        self.entry_names.clear();
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

    /// Load address of the image the backend marked as the main executable
    /// ([`ImageRole::MainExecutable`]), after [`load_images`](Self::load_images). `None`
    /// when no reported image carries that mark, whatever else is listed.
    ///
    /// # Errors
    /// [`DebuggerError::ImageMapping`] when the main image's mapped header could not be read.
    /// [`DebuggerError::InvalidArgument`] when the backend marks more than one image, which
    /// breaks the [`ImageRole`] contract: picking one would resolve breakpoints in a
    /// program chosen by list order.
    pub(super) fn main_load_address(&self) -> Result<Option<Address>>
    {
        let mut mains = self
            .loaded_images
            .iter()
            .filter(|image| image.role == ImageRole::MainExecutable);
        let Some(main) = mains.next() else {
            return Ok(None);
        };
        if let Some(second) = mains.next() {
            return Err(DebuggerError::InvalidArgument(format!(
                "the backend marks more than one image as the main executable ({} and {})",
                main.descriptor.path.display(),
                second.descriptor.path.display()
            )));
        }
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
    ///
    /// Within a snapshot, a load address identifies one mapped header and its build id.
    /// In the live probe for #270, relative, absolute, symbolic-link, hard-link and
    /// search-path spellings of one file yielded one listing. A different build with the
    /// same declared image name had its own address. See the platform backend's
    /// image-list documentation for the probe results and loader behavior.
    ///
    /// If a backend lists two spellings of one mapping, a listing whose id is unknown
    /// takes the id from the other listing at that address (#249). Two spellings that
    /// resolve to one [`SymbolCache`] entry both stop serving the opened file when the
    /// id contradicts it ([`Self::forget_contradicted_file`]), and `retain_ids` drops
    /// the entry.
    pub(super) fn load_images(&mut self) -> Result<()>
    {
        let images = match self.backend.images() {
            Ok(images) => images,
            Err(BackendError::Unsupported(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut mapped_ids = HashMap::new();
        for image in &images {
            if let Some(id) = image.build_id.known() {
                mapped_ids.entry(image.load_address.value()).or_insert(id);
            }
        }
        let mut previous = std::mem::take(&mut self.loaded_images);
        let mut changed = false;
        for image in images {
            let descriptor = ImageDescriptor {
                path: image.path,
                load_address: image.load_address.value(),
            };
            let build_id = image
                .build_id
                .or_later(BuildIdentity::from(mapped_ids.get(&descriptor.load_address).copied()));
            if let Some(index) = previous.iter().position(|entry| {
                entry.descriptor.path == descriptor.path
                    && entry.descriptor.load_address == descriptor.load_address
                    && same_image(entry.build_id, entry.generation, build_id, self.stop_generation)
            }) {
                let mut kept = previous.swap_remove(index);
                let updated_build = kept.build_id.or_later(build_id);
                if kept.symbols.is_none() && updated_build != kept.build_id {
                    // The known id can select a different part of the same file.
                    kept.failed_load = None;
                }
                kept.build_id = updated_build;
                kept.role = image.role;
                kept.slice = kept.slice.map(|slice| slice.or_later(kept.build_id));
                self.forget_contradicted_file(&mut kept);
                self.loaded_images.push(kept);
            } else {
                changed = true;
                let MappedImage { mapping, slice } = self.map_loaded_image(descriptor.load_address, build_id);
                self.loaded_images.push(SessionImage {
                    descriptor,
                    role: image.role,
                    generation: self.stop_generation,
                    build_id: build_id.or_later(slice.as_ref().map_or(BuildIdentity::Unknown, ImageSlice::build_id)),
                    mapping,
                    slice,
                    symbols: None,
                    failed_load: None,
                    symtab: None,
                    symtab_failed_in_request: None,
                });
            }
        }
        if changed || !previous.is_empty() {
            // A name, or the lack of one, may come from an image that is gone or new.
            self.entry_names.clear();
        }
        self.symbols
            .retain_ids(&self.loaded_images.iter().filter_map(|image| image.symbols).collect());
        Ok(())
    }

    /// Stop serving the file `image` was loaded from when its build is not the mapped
    /// one (#221).
    ///
    /// A file opened while the mapped build id was unknown was read without a check of
    /// its build. Once the id is known, the build the file records is compared with it
    /// ([`same_build`]): a file of another build, or one recording no id, loses both
    /// halves, its unwind tables and its source debug information, along with what the
    /// session learned from loading it, so the next lookup opens the file again for the
    /// known build. A file of the same build, or one whose id the format could not tell,
    /// is kept without being opened again. The mapping and the symbol table come from the
    /// target's memory, not from the file, and are kept either way.
    fn forget_contradicted_file(&self, image: &mut SessionImage)
    {
        let Some(id) = image.symbols else {
            return;
        };
        let Some(file) = self.symbols.file_build_id(id) else {
            return;
        };
        if image.build_id.known().is_none() || same_build(file, image.build_id) {
            return;
        }
        tracing::debug!(
            "{} at 0x{:x}: the opened file is another build ({file:?}) than the mapped one ({:?}); reading it again",
            image.descriptor.path.display(),
            image.descriptor.load_address,
            image.build_id
        );
        image.symbols = None;
        image.failed_load = None;
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
    /// failed is not read again. The next request checks whether its inputs changed.
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
    /// [`DebuggerError::NoDebugInfo`] when the image has no usable DWARF, or
    /// [`DebuggerError::SymbolLoad`] with the original failure for any other load error
    /// (see [`FailedLoad`]). An image whose file could be read keeps its unwind tables
    /// loaded either way ([`load_executable_for_address`](Self::load_executable_for_address)).
    pub(super) fn load_symbols_for_address(&mut self, address: Address) -> Result<bool>
    {
        let request = self.symbol_request;
        let loaded = self.load_executable_for_address(address)?;
        let Some(image) = self.image_containing(address) else {
            return Ok(loaded);
        };
        if let Some(failed) = &mut image.failed_load {
            failed.report(request, &image.descriptor.path)?;
        }
        Ok(loaded)
    }

    /// Load the image containing `address` into the [`SymbolCache`] for its layout and
    /// unwind tables, and record the outcome of its source debug information on the
    /// [`SessionImage`] without failing on it. Returns whether the image was newly
    /// loaded: a stack trace walks again then, since the image's unwind tables can reveal
    /// callers that the frame-pointer chain missed.
    ///
    /// After a failure, compare the load's file stamps at most once per request. When
    /// only companion inputs changed, retry discovery over the cached executable.
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
        let inputs = match &mut image.failed_load {
            None if image.symbols.is_some() => return Ok(false),
            None => self.symbols.load_inputs(&image.descriptor, None),
            Some(failed) => {
                if failed.checked == request {
                    return Ok(false);
                }
                failed.checked = request;
                let inputs = self.symbols.load_inputs(&image.descriptor, Some(&failed.inputs));
                if failed.inputs == inputs {
                    if image.symbols.is_none() {
                        failed.report(request, &image.descriptor.path)?;
                    }
                    return Ok(false);
                }
                inputs
            }
        };
        let loaded = match (image.symbols, image.failed_load.as_ref()) {
            (Some(id), Some(failed)) if inputs.executable == failed.inputs.executable => {
                Ok(self.symbols.retry_debug_info(id))
            }
            (previous, failed) => self
                .symbols
                .load_executable(image.descriptor.clone(), image.slice.as_ref(), image.build_id, previous)
                .map(Some)
                .or_else(|error| {
                    // Reopening a changed file can fail while the live mapping and its
                    // companion are still usable. Try a changed companion independently.
                    previous
                        .filter(|_| failed.is_some_and(|failed| inputs.companions != failed.inputs.companions))
                        .and_then(|id| self.symbols.retry_debug_info(id))
                        .map_or(Err(error), |image| Ok(Some(image)))
                }),
        };
        match loaded {
            Ok(None) => {
                // This format has no companion lookup for the cached executable.
                if let Some(failed) = &mut image.failed_load {
                    failed.inputs = inputs;
                }
                Ok(false)
            }
            Ok(Some(loaded)) => {
                image.symbols = Some(loaded.id());
                image.failed_load = loaded.debug_error().map(|error| FailedLoad {
                    reported: None,
                    checked: request,
                    inputs: inputs.with_discovered_companions(&loaded),
                    error: error.duplicate(),
                });
                Ok(true)
            }
            Err(error) => {
                let mut failed = FailedLoad {
                    reported: None,
                    checked: request,
                    inputs,
                    error,
                };
                let result = if image.symbols.is_none() {
                    failed.report(request, &image.descriptor.path)
                } else {
                    Ok(())
                };
                image.failed_load = Some(failed);
                result.map(|()| false)
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
