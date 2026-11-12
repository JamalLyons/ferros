//! Loading the backend's image list into the session's symbol cache.

use super::Session;
use crate::backend::{BackendError, LoadedImage};
use crate::error::Result;
use crate::symbols::ImageDescriptor;

impl Session
{
    /// The main executable and loaded libraries with their load addresses, as reported
    /// by [`Backend::images`](crate::backend::Backend::images).
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`](crate::error::DebuggerError::NotAttached) with no
    /// process, [`DebuggerError::NotStopped`](crate::error::DebuggerError::NotStopped)
    /// while running, or the backend's error.
    pub fn images(&mut self) -> Result<Vec<LoadedImage>>
    {
        self.require_stopped()?;
        Ok(self.backend.images()?)
    }

    /// Give every image the backend reports to the symbol cache.
    ///
    /// Each (path, load address) pair is tried once per process; an image that fails to
    /// load (missing file, unknown format) is skipped and not retried. A backend that
    /// cannot enumerate images ([`BackendError::Unsupported`]) leaves the cache as it is.
    ///
    /// The caller has checked that the session is stopped.
    pub(super) fn load_images(&mut self) -> Result<()>
    {
        let images = match self.backend.images() {
            Ok(images) => images,
            Err(BackendError::Unsupported(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for image in images {
            let load_address = image.load_address.value();
            if !self.loaded_images.insert((image.path.clone(), load_address)) {
                continue;
            }
            if let Err(error) = self.symbols.load_image(ImageDescriptor {
                path: image.path,
                load_address,
            }) {
                tracing::debug!("could not load image at 0x{load_address:x} into the symbol cache: {error}");
            }
        }
        Ok(())
    }
}
