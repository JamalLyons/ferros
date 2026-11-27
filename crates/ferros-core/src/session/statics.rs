//! Statics and globals of the main executable through a stopped session.

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::values::scope::Variable;
use crate::values::statics::{Static, describe, read_static};

impl Session
{
    /// The statics of the main executable whose qualified name contains `filter`
    /// (every static without one), sorted by name. Each carries the runtime address the
    /// loaded image maps it to. Thread-locals are not statics here (see
    /// [`values::statics`](crate::values::statics)).
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
        let types = image.types().ok();
        let filter = filter.map(str::trim).filter(|filter| !filter.is_empty());
        Ok(index
            .entries()
            .iter()
            .filter(|entry| filter.is_none_or(|filter| entry.name.contains(filter)))
            .map(|entry| describe(&image, types, entry))
            .collect())
    }

    /// The main executable's static named `name`, read from the loaded image, as a
    /// [`Variable`] that [`Self::format_variable`] and [`Self::format_path`] render.
    ///
    /// `name` is a qualified name (`my_crate::config::LIMIT`, a leading `::` allowed) or
    /// a trailing part of one on a `::` boundary (`config::LIMIT`, `LIMIT`). `Ok(None)`
    /// when no static has that name.
    ///
    /// # Errors
    ///
    /// As for [`Self::statics`], or [`DebuggerError::InvalidArgument`] naming the
    /// candidates when `name` matches more than one static.
    pub fn static_variable(&mut self, name: &str) -> Result<Option<Variable>>
    {
        self.require_stopped()?;
        let image = self.main_image()?;
        let found = image.statics()?.lookup(name);
        let entry = match found.as_slice() {
            [] => return Ok(None),
            [entry] => *entry,
            several => {
                let names: Vec<_> = several.iter().map(|entry| entry.name.as_str()).collect();
                return Err(DebuggerError::InvalidArgument(format!(
                    "'{name}' is ambiguous: it names {}",
                    names.join(", ")
                )));
            }
        };
        Ok(Some(read_static(&image, entry, |address, len| {
            self.read_memory(address, len)
        })))
    }
}
