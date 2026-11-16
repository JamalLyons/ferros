//! Breakpoint locations: what a user asks for, and the addresses it resolves to.
//!
//! A [`BreakpointLocation`] is a source line, a function name or a raw address. The
//! session resolves it against the main executable's debug info (the first image the
//! backend reports, which is where a Rust program's own code and its statically linked
//! `std` live) into one [`BreakpointSite`] per address:
//!
//! - `file:line` takes every statement address of the first line at or after `line` that
//!   has code ([`BinaryImage::line_addresses`]). The line used is reported, so a line with
//!   no code shows where the breakpoint really is.
//! - A function name takes the post-prologue address of every matching function
//!   ([`BinaryImage::function_addresses`]), one per generic instantiation.
//! - An address is used as is.
//!
//! Each site records the [`SourcePosition`] of its address, taken from the address's
//! symbolication: the frame of the function the address belongs to, not the innermost
//! line-table row, which can be an inlined `std` function. A stop at the site reports that
//! same position, so the file a breakpoint resolved to and the file of its stop are always
//! spelled the same way (normalized as the line index normalizes paths).

use std::fmt;
use std::sync::Arc;

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::symbols::BinaryImage;
use crate::symbols::lines::normalize_path;
use crate::types::Address;

/// Where a user asked for a breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointLocation
{
    /// A source line. `file` is a full path or a path suffix matched on component
    /// boundaries (`main.rs`, `src/main.rs`); `line` is 1-based.
    Line
    {
        /// The file, as given.
        file: String,
        /// The requested line.
        line: u32,
    },
    /// A Rust function path, matched on `::` boundaries against the trailing segments of
    /// each function's path (`parse`, `my_crate::parse`, `Counter::bump`).
    Function(String),
    /// A runtime address.
    Address(Address),
}

impl fmt::Display for BreakpointLocation
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        match self {
            Self::Line { file, line } => write!(f, "{file}:{line}"),
            Self::Function(name) => f.write_str(name),
            Self::Address(address) => write!(f, "*{:#x}", address.value()),
        }
    }
}

/// The function and source line of an address, each when known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePosition
{
    /// Demangled function name, without the hash.
    pub function: Option<String>,
    /// Source file, normalized (empty and `.` components dropped).
    pub file: Option<String>,
    /// 1-based source line.
    pub line: Option<u32>,
}

/// One address a breakpoint resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointSite
{
    /// Runtime address of the trap.
    pub address: Address,
    /// Function and source line of `address`.
    pub position: SourcePosition,
}

impl Session
{
    /// Resolve `location` to its sites, sorted by address. The process must be stopped.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::SourceLookup`] for an unknown or ambiguous file or function, a
    /// line past the file's last line with code, or unreadable debug info; the main
    /// image's symbol-loading error ([`DebuggerError::NoDebugInfo`],
    /// [`DebuggerError::SymbolLoad`], [`DebuggerError::ImageMapping`]); or
    /// [`DebuggerError::InvalidArgument`] when the process reports no image.
    pub(super) fn resolve_location(&mut self, location: &BreakpointLocation) -> Result<Vec<BreakpointSite>>
    {
        let mut sites = match location {
            BreakpointLocation::Address(address) => vec![BreakpointSite {
                address: *address,
                position: self.position_at(*address, None),
            }],
            BreakpointLocation::Line { file, line } => {
                let found = self.main_image()?.line_addresses(file, *line)?;
                found
                    .addresses
                    .iter()
                    .map(|&address| {
                        let mut position = self.position_at(address, Some((&found.file, found.line)));
                        // The line table decides the line: it is the one that has code.
                        position.file = Some(found.file.clone());
                        position.line = Some(found.line);
                        BreakpointSite { address, position }
                    })
                    .collect()
            }
            BreakpointLocation::Function(name) => {
                let mut addresses: Vec<Address> = self
                    .main_image()?
                    .function_addresses(name)?
                    .into_iter()
                    .map(|function| function.after_prologue)
                    .collect();
                addresses.sort_unstable();
                addresses.dedup();
                addresses
                    .into_iter()
                    .map(|address| BreakpointSite {
                        address,
                        position: self.position_at(address, None),
                    })
                    .collect()
            }
        };
        sites.sort_by_key(|site| site.address);
        sites.dedup_by_key(|site| site.address);
        Ok(sites)
    }

    /// The main executable's symbols (the first image the backend reports).
    fn main_image(&mut self) -> Result<Arc<BinaryImage>>
    {
        self.load_images()?;
        self.begin_symbol_request();
        let load_address = self
            .main_load_address()?
            .ok_or_else(|| DebuggerError::InvalidArgument("the process reports no executable image".into()))?;
        self.load_symbols_for_address(load_address)?;
        self.symbols
            .image_for_address(load_address)
            .ok_or_else(|| DebuggerError::InvalidArgument(format!("no symbols are loaded for the image at {load_address}")))
    }

    /// Function and source line of `address`, from the frame of the function that owns
    /// it (not an inlined callee), with the file normalized as the line index does.
    ///
    /// For an address that is a breakpoint site, the site's own
    /// [`position`](BreakpointSite::position) is the one to show: it is what the breakpoint
    /// reported when it was set. This is for any other address.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running. Missing symbols give an empty position, not an error.
    pub fn source_position(&mut self, address: Address) -> Result<SourcePosition>
    {
        self.require_stopped()?;
        Ok(self.position_at(address, None))
    }

    /// Function and source line of `address`, best effort.
    ///
    /// The frame used is the innermost one at `prefer` (a normalized file and a line),
    /// when given and present, else the frame of the function that owns the address (the
    /// last, physical frame of the symbolication; the frames before it are inlined
    /// callees). An address without symbols has an empty position.
    pub(super) fn position_at(&mut self, address: Address, prefer: Option<(&str, u32)>) -> SourcePosition
    {
        let symbolication = match self.symbolicate(address) {
            Ok(Some(symbolication)) => symbolication,
            Ok(None) => return SourcePosition::default(),
            Err(error) => {
                tracing::debug!("no source position for {address}: {error}");
                return SourcePosition::default();
            }
        };
        let preferred = prefer.and_then(|(file, line)| {
            symbolication.frames.iter().find(|frame| {
                frame
                    .location
                    .as_ref()
                    .is_some_and(|location| location.line == Some(line) && normalize_path(&location.file) == file)
            })
        });
        let Some(frame) = preferred.or_else(|| symbolication.frames.last()) else {
            return SourcePosition::default();
        };
        SourcePosition {
            function: Some(frame.symbol.display_name().to_owned()),
            file: frame.location.as_ref().map(|location| normalize_path(&location.file)),
            line: frame.location.as_ref().and_then(|location| location.line),
        }
    }
}
