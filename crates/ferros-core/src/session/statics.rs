//! Statics and globals of the main executable through a stopped session.

use std::fmt::Write as _;

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::values::statics::{Static, read_static};

impl Session
{
    /// The statics of the main executable whose qualified name contains `filter`
    /// (every static without one), sorted by name, each read from the loaded image at
    /// the runtime address it maps the static to. Statics that share a qualified name are
    /// separate entries, in address order. Thread-locals are not statics here (see
    /// [`values::statics`](crate::values::statics)).
    ///
    /// The image list and the static index are read once per call, and each static's
    /// bytes with one memory read; a value that cannot be read is unavailable on its own
    /// entry.
    ///
    /// # Errors
    ///
    /// A state error unless stopped, as for [`Self::images`]; the main image's symbol
    /// loading error; or [`DebuggerError::SourceLookup`] when its debug information cannot
    /// be read.
    pub fn statics(&mut self, filter: Option<&str>) -> Result<Vec<Static>>
    {
        self.require_stopped()?;
        let image = self.main_image()?;
        let index = image.statics()?;
        let filter = filter.map(str::trim).filter(|filter| !filter.is_empty());
        Ok(index
            .entries()
            .iter()
            .filter(|entry| filter.is_none_or(|filter| entry.name.contains(filter)))
            .map(|entry| read_static(&image, entry, |address, len| self.read_memory(address, len)))
            .collect())
    }

    /// The main executable's static named `name`, read from the loaded image.
    /// [`Static::variable`] is named by the qualified name, for [`Self::format_variable`]
    /// and [`Self::format_path`].
    ///
    /// `name` is a qualified name (`my_crate::config::LIMIT`, a leading `::` allowed) or
    /// a trailing part of one on a `::` boundary (`config::LIMIT`, `LIMIT`). `Ok(None)`
    /// when no static has that name.
    ///
    /// # Errors
    ///
    /// As for [`Self::statics`], or [`DebuggerError::InvalidArgument`] when `name`
    /// matches more than one static, listing each with its runtime address.
    pub fn static_variable(&mut self, name: &str) -> Result<Option<Static>>
    {
        self.require_stopped()?;
        let image = self.main_image()?;
        let found = image.statics()?.lookup(name);
        let entry = match found.as_slice() {
            [] => return Ok(None),
            [entry] => *entry,
            several => {
                let mut candidates = String::new();
                for (i, entry) in several.iter().enumerate() {
                    let separator = if i == 0 { "" } else { ", " };
                    let _ = write!(candidates, "{separator}{}", entry.name);
                    match image.runtime_address(entry.file_address) {
                        Some(address) => _ = write!(candidates, " @ {address}"),
                        None => _ = write!(candidates, " @ file address {:#x} (unmapped)", entry.file_address),
                    }
                }
                return Err(DebuggerError::InvalidArgument(format!(
                    "'{name}' is ambiguous: it names {candidates}"
                )));
            }
        };
        Ok(Some(read_static(&image, entry, |address, len| {
            self.read_memory(address, len)
        })))
    }
}
