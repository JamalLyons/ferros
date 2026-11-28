//! Variable inspection through a stopped session.

use std::cell::RefCell;
use std::sync::{Arc, PoisonError};

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::symbols::BinaryImage;
use crate::types::Address;
use crate::values::format::{FormatOptions, ValueFormatter, ValueView, format_raw};
use crate::values::location::{Unavailable, ValueLocation};
use crate::values::path::{Path, PathError, Place, Storage, evaluate};
use crate::values::pretty::{LockPrinter, PRINTERS, PrettyPrinter};
use crate::values::scope::{Variable, frame_variables};
use crate::values::types::{DieOffset, TypeId};

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
        let text = self.format_storage(&image, ty, &Storage::Bytes(bytes.to_vec(), memory_address(variable)), options);
        (name, text)
    }

    /// Evaluate `path` from `root`, the variable named by [`Path::root`], and render the
    /// value it selects. Returns the value's type name, if known, and its text. A path
    /// that is only the variable's name renders as [`Self::format_variable`].
    ///
    /// # Errors
    ///
    /// A [`PathError`] naming the segment that cannot be evaluated: an unknown field, an
    /// index out of bounds, a null dereference, an unreadable pointer or a value of the
    /// wrong kind.
    pub fn format_path(
        &mut self,
        root: &Variable,
        path: &Path,
        options: FormatOptions,
    ) -> std::result::Result<(Option<String>, String), PathError>
    {
        if path.is_variable() {
            return Ok(self.format_variable(root, options));
        }
        let unknown = || PathError::root(path, "type unknown");
        let image = self.symbols.image_by_id(root.image_id).ok_or_else(unknown)?;
        let types = image.types().map_err(|_| unknown())?;
        let ty = root
            .type_offset
            .and_then(|offset| types.type_at(DieOffset(offset.0 as u64)))
            .ok_or_else(unknown)?;
        let bytes = root
            .value
            .bytes
            .as_ref()
            .map_err(|reason| PathError::root(path, reason.to_string()))?;
        let root_place = Place {
            ty,
            storage: Storage::Bytes(bytes.clone(), memory_address(root)),
        };
        let place = {
            let mut read = |address, len| self.read_memory(address, len);
            evaluate(path, root_place, types, image.endian(), &mut read)?
        };
        let name = types.get(place.ty).and_then(|ty| ty.name.clone());
        let text = self.format_storage(&image, place.ty, &place.storage, options);
        Ok((name, text))
    }

    fn format_storage(&mut self, image: &Arc<BinaryImage>, ty: TypeId, storage: &Storage, options: FormatOptions) -> String
    {
        let Ok(types) = image.types() else {
            return format_raw(Err(&Unavailable::SizeUnknown));
        };
        self.begin_symbol_request();
        // Lock printers ask the platform runtime about system mutexes.
        let runtime = Arc::clone(&self.runtime);
        let locks = LockPrinter::with_runtime(runtime.as_ref());
        let printers: Vec<&dyn PrettyPrinter> = std::iter::once(&locks as &dyn PrettyPrinter)
            .chain(PRINTERS.iter().copied())
            .collect();
        let session = RefCell::new(self);
        let mut read = |address, len| session.borrow_mut().read_memory(address, len);
        let mut image_lookup = |address| {
            let mut session = session.borrow_mut();
            session.require_stopped().ok()?;
            let _ = session.load_symbols_for_address(address);
            session.symbols.image_for_address(address)
        };
        let mut formatter = ValueFormatter::new(types, image.endian(), options, &mut read)
            .with_image_lookup(&mut image_lookup)
            .with_printers(&printers);
        match storage {
            Storage::Bytes(bytes, address) => formatter.format(ValueView {
                ty,
                bytes,
                address: *address,
            }),
            Storage::Memory(address) => formatter.format_at(ty, *address),
        }
    }
}

fn memory_address(variable: &Variable) -> Option<Address>
{
    match variable.value.location {
        Some(ValueLocation::Memory(address)) => Some(address),
        _ => None,
    }
}
