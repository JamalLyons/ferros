//! The selected stack frame.
//!
//! The selection is an index into the selected thread's backtrace, innermost first.
//! Inline frames count, in the same order as
//! [`Session::stack_trace`](super::Session::stack_trace). It is `0` after launch and
//! attach, after every resume, after every stop, and after
//! [`Session::select_thread`](super::Session::select_thread).

use super::Session;
use crate::error::{DebuggerError, Result};
use crate::types::StackFrame;

impl Session
{
    /// Forget a frame chosen for the previous stop or thread.
    pub(super) fn reset_selected_frame(&mut self)
    {
        self.selected_frame = 0;
    }

    /// Index of the selected frame on the selected thread.
    ///
    /// `0` is the innermost frame. Inline frames count, in the same order as
    /// [`stack_trace`](Self::stack_trace). The index is `0` after [`launch`](Self::launch),
    /// [`attach`](Self::attach), every resume and step, every stop, and
    /// [`select_thread`](Self::select_thread). Reading it does not require a stopped
    /// process: while the process is running the index is already `0`.
    #[must_use]
    pub fn selected_frame(&self) -> usize
    {
        self.selected_frame
    }

    /// Select frame `index` of the selected thread.
    ///
    /// `0` is the innermost frame. Inline frames count, as in
    /// [`stack_trace`](Self::stack_trace). An index past the last frame leaves the
    /// selection unchanged.
    ///
    /// # Errors
    ///
    /// [`DebuggerError::NotAttached`] with no process, [`DebuggerError::NotStopped`]
    /// while running, [`DebuggerError::NoThreadSelected`] if the process has no
    /// thread, [`DebuggerError::InvalidArgument`] if `index` is past the last frame,
    /// or an error from unwinding the stack.
    pub fn select_frame(&mut self, index: usize) -> Result<()>
    {
        self.frames_through(index)?;
        self.selected_frame = index;
        Ok(())
    }

    /// The selected frame of the selected thread.
    ///
    /// This is [`stack_trace`](Self::stack_trace)'s frame at
    /// [`selected_frame`](Self::selected_frame). Later commands that show a value read
    /// this frame.
    ///
    /// # Errors
    ///
    /// As for [`select_frame`](Self::select_frame), including
    /// [`DebuggerError::InvalidArgument`] when the stored index is past the frames the
    /// stack now has.
    pub fn selected_stack_frame(&mut self) -> Result<StackFrame>
    {
        let index = self.selected_frame;
        let mut frames = self.frames_through(index)?;
        Ok(frames.swap_remove(index))
    }

    /// The selected thread's frames through `index`, or an error when `index` is past
    /// the last one. The selection is not changed.
    fn frames_through(&mut self, index: usize) -> Result<Vec<StackFrame>>
    {
        self.require_stopped()?;
        self.selected_or_error()?;
        let frames = self.stack_trace(index.saturating_add(1))?;
        if index >= frames.len() {
            return Err(DebuggerError::InvalidArgument(format!(
                "no frame {index} (the stack has {} frames)",
                frames.len()
            )));
        }
        Ok(frames)
    }
}
