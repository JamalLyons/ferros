//! Breakpoint locations: what a user asks for, and the addresses it resolves to.
//!
//! A [`BreakpointLocation`] is a source line, a function name or a raw address. The
//! session resolves it against the main executable's debug info (the image the backend
//! marks as the main executable, wherever it is listed, which is where a Rust program's own
//! code and its statically linked `std` live) into one [`BreakpointSite`] per address:
//!
//! - `file:line` takes every statement address of the first line at or after `line` that
//!   has code ([`BinaryImage::line_addresses`]). The line used is reported, so a line with
//!   no code shows where the breakpoint really is.
//! - A function name takes the post-prologue address of every matching function
//!   ([`BinaryImage::function_addresses`]), one per generic instantiation.
//! - An address is used as is.
//!
//! A list that marks no image as the main executable resolves no file or function
//! location: a library is never searched in its place.
//!
//! Each site records the [`SourcePosition`] of its address, taken from the address's
//! symbolication: the frame of the function the address belongs to, not the innermost
//! line-table row, which can be an inlined `std` function. A stop at the site reports that
//! same position, so the file a breakpoint resolved to and the file of its stop are always
//! spelled the same way (normalized as the line index normalizes paths). A file or
//! function location reads the image list once: its sites are positioned in the symbol
//! request that found them, without enumerating the images again per site.

use std::fmt;
use std::sync::Arc;

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::symbols::lines::normalize_path;
use crate::symbols::{BinaryImage, SymbolFrame};
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
    /// [`DebuggerError::InvalidArgument`] when the process marks no image as its main
    /// executable.
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
                        let position = self.position_in_request(address, Some((&found.file, found.line)));
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
                        position: self.position_in_request(address, None),
                    })
                    .collect()
            }
        };
        sites.sort_by_key(|site| site.address);
        sites.dedup_by_key(|site| site.address);
        Ok(sites)
    }

    /// The main executable's symbols (the image the backend marks as the main executable).
    /// Refreshes the image list and starts the symbol request that the location's sites
    /// are then positioned in ([`position_in_request`](Self::position_in_request)).
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
    /// Without `prefer`, the frame is the one of the function that owns the address (the
    /// last, physical frame of the symbolication; the frames before it are inlined
    /// callees). With `prefer` (a normalized file and a line from the line table), see
    /// [`site_position`]. An address without symbols has an empty position.
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
        position_from(&symbolication.frames, prefer)
    }

    /// [`position_at`](Self::position_at) within the current symbol request, over the
    /// images loaded at its start: no new image enumeration and no new request. An image
    /// whose load failed earlier in the request is not read again, and the address then
    /// has an empty position.
    fn position_in_request(&mut self, address: Address, prefer: Option<(&str, u32)>) -> SourcePosition
    {
        if let Err(error) = self.load_symbols_for_address(address) {
            tracing::debug!("no source position for {address}: {error}");
            return SourcePosition::default();
        }
        self.symbols
            .symbolicate(address)
            .map_or_else(SourcePosition::default, |symbolication| {
                position_from(&symbolication.frames, prefer)
            })
    }
}

/// The position of an address whose symbolication is `frames`: see
/// [`Session::position_at`].
fn position_from(frames: &[SymbolFrame], prefer: Option<(&str, u32)>) -> SourcePosition
{
    match prefer {
        Some((file, line)) => site_position(frames, file, line),
        None => frames.last().map_or_else(SourcePosition::default, frame_position),
    }
}

/// The position of a `file:line` site whose line-table row is `file` (normalized) and
/// `line`, from `frames` (innermost first, as symbolication returns them).
///
/// The line table decides the file and the line: that is where the code is. The function
/// is that of the innermost frame at `file` and `line`, else of the innermost frame in
/// `file` (an inlined callee whose frame line differs from the row), else of the innermost
/// frame. It is never the outer function when an inlined callee holds the row's file, so
/// the function named is one whose source contains the line.
fn site_position(frames: &[SymbolFrame], file: &str, line: u32) -> SourcePosition
{
    let in_file = |frame: &&SymbolFrame| {
        frame
            .location
            .as_ref()
            .is_some_and(|location| normalize_path(&location.file) == file)
    };
    let frame = frames
        .iter()
        .filter(in_file)
        .find(|frame| frame.location.as_ref().is_some_and(|location| location.line == Some(line)))
        .or_else(|| frames.iter().find(in_file))
        .or_else(|| frames.first());
    SourcePosition {
        function: frame.map(|frame| frame.symbol.display_name().to_owned()),
        file: Some(file.to_owned()),
        line: Some(line),
    }
}

/// The position a symbol frame reports.
fn frame_position(frame: &SymbolFrame) -> SourcePosition
{
    SourcePosition {
        function: Some(frame.symbol.display_name().to_owned()),
        file: frame.location.as_ref().map(|location| normalize_path(&location.file)),
        line: frame.location.as_ref().and_then(|location| location.line),
    }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::types::{SourceLocation, SymbolLanguage, SymbolName};

    fn frame(function: &str, file: &str, line: u32) -> SymbolFrame
    {
        SymbolFrame {
            symbol: SymbolName::from_debug_info(function.to_owned(), None, SymbolLanguage::Rust),
            location: Some(SourceLocation {
                file: file.to_owned(),
                line: Some(line),
                column: None,
            }),
            parameters: Vec::new(),
        }
    }

    /// Issue #108: the row is in the inlined callee's file at a line its frame does not
    /// report; the site names the callee, not the caller, with the row's file and line.
    #[test]
    fn an_inlined_callee_in_the_rows_file_names_the_site()
    {
        let frames = [frame("inlined", "/src/callee.rs", 12), frame("caller", "/src/main.rs", 40)];
        let position = site_position(&frames, "/src/callee.rs", 10);
        assert_eq!(position.function.as_deref(), Some("inlined"));
        assert_eq!(position.file.as_deref(), Some("/src/callee.rs"));
        assert_eq!(position.line, Some(10));
    }

    /// An exact file and line match wins over an inner frame in the same file.
    #[test]
    fn an_exact_match_wins()
    {
        let frames = [
            frame("inner", "/src/main.rs", 5),
            frame("middle", "/src/main.rs", 10),
            frame("outer", "/src/main.rs", 40),
        ];
        let position = site_position(&frames, "/src/main.rs", 10);
        assert_eq!(position.function.as_deref(), Some("middle"));
        assert_eq!(position.line, Some(10));
    }

    /// The file is compared normalized, as the line index spells it.
    #[test]
    fn frame_files_are_normalized()
    {
        let frames = [frame("inlined", "/src/./callee.rs", 12), frame("caller", "/src/main.rs", 40)];
        let position = site_position(&frames, "/src/callee.rs", 10);
        assert_eq!(position.function.as_deref(), Some("inlined"));
    }

    /// No frame in the row's file: the innermost frame names the site.
    #[test]
    fn no_frame_in_the_file_uses_the_innermost()
    {
        let frames = [frame("inlined", "/std/num.rs", 3), frame("caller", "/src/main.rs", 40)];
        let position = site_position(&frames, "/src/other.rs", 7);
        assert_eq!(position.function.as_deref(), Some("inlined"));
        assert_eq!(position.file.as_deref(), Some("/src/other.rs"));
        assert_eq!(position.line, Some(7));
    }
}
