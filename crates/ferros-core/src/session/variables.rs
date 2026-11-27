//! Variable inspection through a stopped session.

use std::sync::PoisonError;

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::values::scope::{Variable, frame_variables};

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
            .map_err(|error| debug_info_error(error.to_string()))?
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
}
