//! Variable inspection through a stopped session.

use std::cell::RefCell;
use std::sync::PoisonError;

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::values::format::{FormatOptions, ValueFormatter, ValueView, format_raw};
use crate::values::location::ValueLocation;
use crate::values::scope::{Variable, frame_variables};
use crate::values::types::DieOffset;

impl Session
{
    /// Read the parameters and locals in scope for the selected logical frame.
    ///
    /// Frame selection includes inline frames. A caller's scopes and location lists
    /// are evaluated at the call instruction. A frame without source debug information
    /// returns an empty list. Unavailable values have a reason on their own entry.
    /// Arguments precede locals, and outer lexical scopes precede nested scopes.
    ///
    /// # Errors
    ///
    /// As for [`Self::selected_stack_frame`], or when frame scopes cannot be read from
    /// the image's debug information.
    pub fn selected_frame_variables(&mut self) -> Result<Vec<Variable>>
    {
        self.frame_variables(self.selected_frame())
    }

    /// Read the parameters and locals in scope for `index` of the selected thread.
    ///
    /// `index` uses [`Self::stack_trace`] ordering. The selected frame is unchanged.
    ///
    /// # Errors
    ///
    /// As for [`Self::selected_frame_variables`].
    pub fn frame_variables(&mut self, index: usize) -> Result<Vec<Variable>>
    {
        let trace = self.stack_trace_with_registers(index.saturating_add(1))?;
        let frame = trace.frames().get(index).ok_or_else(|| {
            DebuggerError::InvalidArgument(format!("no frame {index} (the stack has {} frames)", trace.frames().len()))
        })?;
        let pc = trace.lookup_address(index).expect("a stack frame has a lookup address");
        let regs = trace.frame_registers(index).expect("a stack frame has register state");
        let Some(image) = self.symbols.image_for_address(pc) else {
            return Ok(Vec::new());
        };
        if !image.has_debug_info() {
            return Ok(Vec::new());
        }
        let Some(file_pc) = image.file_address(pc) else {
            return Ok(Vec::new());
        };
        let debug_info_error = |detail| DebuggerError::NoDebugInfo {
            path: image.path().to_path_buf(),
            detail,
        };
        let context = image
            .symbol_context()
            .map_err(|error| match error {
                DebuggerError::NoDebugInfo { .. } => error,
                other => debug_info_error(other.to_string()),
            })?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        frame_variables(
            &context,
            file_pc,
            frame.kind,
            image.id(),
            regs,
            |address, len| self.read_memory(address, len),
            |address| image.runtime_address(address),
        )
        .map_err(|error| debug_info_error(format!("failed to walk variable DWARF: {error}")))
    }

    /// Render a variable from the current stop. Returns its type name, if known,
    /// and its value text. Readable bytes fall back to hex when the type is missing.
    pub fn format_variable(&mut self, variable: &Variable, options: FormatOptions) -> (Option<String>, String)
    {
        let bytes = variable.value.bytes.as_deref();
        let fallback = || (variable.type_name.clone(), format_raw(bytes));
        let Some(image) = self.symbols.image_by_id(variable.image_id) else {
            return fallback();
        };
        let Ok(types) = image.types() else {
            return fallback();
        };
        let Some(ty) = variable
            .type_offset
            .and_then(|offset| types.type_at(DieOffset(offset.0 as u64)))
        else {
            return fallback();
        };
        let name = types
            .get(ty)
            .and_then(|ty| ty.name.clone())
            .or_else(|| variable.type_name.clone());
        let Ok(bytes) = bytes else {
            return (name, format_raw(bytes));
        };
        let address = match variable.value.location {
            Some(ValueLocation::Memory(address)) => Some(address),
            _ => None,
        };
        self.begin_symbol_request();
        let session = RefCell::new(self);
        let mut read = |address, len| session.borrow_mut().read_memory(address, len);
        let mut image_lookup = |address| {
            let mut session = session.borrow_mut();
            session.require_stopped().ok()?;
            let _ = session.load_symbols_for_address(address);
            session.symbols.image_for_address(address)
        };
        let text = ValueFormatter::new(types, image.endian(), options, &mut read)
            .with_image_lookup(&mut image_lookup)
            .format(ValueView { ty, bytes, address });
        (name, text)
    }
}
