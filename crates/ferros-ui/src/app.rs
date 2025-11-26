//! Application state and logic

use std::collections::VecDeque;
use std::fmt::Write;

use ferros_core::events::{DebuggerEvent, format_stop_reason};
use ferros_core::types::{Address, FrameId, SourceLocation, StackFrame, StopReason, ThreadId};
use ferros_core::{BreakpointId, BreakpointInfo, Debugger};
use ratatui::widgets::TableState;

/// Maximum number of process output lines retained in memory.
const MAX_PROCESS_OUTPUT_LINES: usize = 4096;
/// Maximum number of debugger stop events retained.
const MAX_STOP_EVENTS: usize = 128;
/// Maximum number of timeline log entries retained.
const MAX_TIMELINE_ENTRIES: usize = 256;

/// Indicates which stream produced a captured line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutputSource
{
    Stdout,
    Stderr,
}

/// Captured line of process output.
#[derive(Debug, Clone)]
pub struct ProcessOutputLine
{
    pub source: ProcessOutputSource,
    pub text: String,
}

/// Application state
#[allow(clippy::struct_excessive_bools)]
pub struct App
{
    /// The debugger instance
    pub debugger: Box<dyn Debugger>,
    /// Process ID of the attached process (if any)
    pub pid: Option<u32>,
    /// Whether this process was launched by us (vs attached to existing)
    pub was_launched: bool,
    /// Whether the application should exit
    pub should_quit: bool,
    /// Current view mode
    pub view_mode: ViewMode,
    /// State for the registers table
    pub registers_state: TableState,
    /// State for the threads table
    pub threads_state: TableState,
    /// State for the memory regions table
    pub memory_regions_state: TableState,
    /// Currently selected thread index
    pub selected_thread_index: usize,
    /// Error message to display (if any)
    pub error_message: Option<String>,
    /// Success/info message to display (if any) - cleared after a short time
    pub info_message: Option<String>,
    /// Timestamp when info message was set (for auto-clearing)
    pub info_message_time: Option<std::time::Instant>,
    /// Process output buffer (for displaying in TUI)
    pub process_output: VecDeque<ProcessOutputLine>,
    /// Number of lines scrolled back from the end of the output buffer
    pub output_scrollback: usize,
    /// Timestamp of last thread list refresh (to avoid refreshing too frequently)
    last_thread_refresh: std::time::Instant,
    /// Whether the target is currently stopped.
    pub target_is_stopped: bool,
    /// Last reported stop reason.
    pub last_stop_reason: StopReason,
    /// Recent stop/resume events for display.
    pub stop_event_log: VecDeque<String>,
    /// Command palette input buffer
    pub command_input: String,
    /// Whether command palette is active
    pub command_palette_active: bool,
    /// Command history
    pub command_history: VecDeque<String>,
    /// Current command history index
    pub command_history_index: Option<usize>,
    /// Cached stack trace for the active thread
    pub cached_stack_trace: Option<Vec<StackFrame>>,
    /// Selected frame ID in the stack view
    pub selected_frame_id: Option<FrameId>,
    /// State for the stack frames table
    pub stack_frames_state: TableState,
    /// State for the breakpoints table
    pub breakpoints_state: TableState,
    /// Cached breakpoints list
    pub cached_breakpoints: Vec<BreakpointInfo>,
    /// Cache of breakpoint addresses to source locations (for UI indicators)
    pub breakpoint_locations: std::collections::HashMap<Address, Option<SourceLocation>>,
    /// Source code cache (file path -> lines)
    pub source_cache: std::collections::HashMap<String, Vec<String>>,
    /// Current source file being displayed
    pub current_source_file: Option<String>,
    /// Source view scroll position (line number)
    pub source_scroll: usize,
    /// Timeline log entries (chronological events)
    pub timeline_log: VecDeque<TimelineEntry>,
    /// Current layout preset
    pub layout_preset: LayoutPreset,
    /// Breakpoint editor state
    pub breakpoint_editor: Option<BreakpointEditorState>,
}

/// Timeline log entry
#[derive(Debug, Clone)]
pub struct TimelineEntry
{
    pub timestamp: std::time::Instant,
    pub kind: TimelineEntryKind,
    pub message: String,
}

/// Timeline entry kind
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineEntryKind
{
    Stop,
    Resume,
    BreakpointHit,
    Signal,
    Output,
    Error,
}

/// Breakpoint editor state
#[derive(Debug, Clone)]
pub struct BreakpointEditorState
{
    pub address_input: String,
    pub kind_input: String, // "software", "hardware", "watchpoint"
    pub watch_length_input: String,
    pub watch_access_input: String, // "read", "write", "readwrite"
    pub editing_existing: Option<BreakpointId>,
}

/// Different view modes in the TUI
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode
{
    /// Overview mode showing debugger info
    Overview,
    /// Registers view
    Registers,
    /// Threads view
    Threads,
    /// Memory regions view
    MemoryRegions,
    /// Process output view
    Output,
    /// Source code + breakpoint view
    Source,
    /// Call stack + frame locals view
    Stack,
    /// Timeline/log panel
    Timeline,
    /// Help view showing keyboard shortcuts and commands
    Help,
}

/// Layout preset for different screen sizes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPreset
{
    /// Compact layout for small terminals
    Compact,
    /// Standard layout for medium terminals
    Standard,
    /// Widescreen layout for large terminals
    Widescreen,
}

impl App
{
    /// Create a new application instance
    #[must_use]
    pub fn new(debugger: Box<dyn Debugger>, pid: Option<u32>, was_launched: bool) -> Self
    {
        let initial_is_stopped = debugger.is_stopped();
        let initial_stop_reason = if initial_is_stopped {
            debugger.stop_reason()
        } else {
            StopReason::Running
        };

        let mut registers_state = TableState::default();
        registers_state.select(Some(0));

        let mut threads_state = TableState::default();
        threads_state.select(Some(0));

        let mut memory_regions_state = TableState::default();
        memory_regions_state.select(Some(0));

        let mut stack_frames_state = TableState::default();
        stack_frames_state.select(Some(0));

        let mut breakpoints_state = TableState::default();
        breakpoints_state.select(Some(0));

        let mut app = Self {
            debugger,
            pid,
            was_launched,
            should_quit: false,
            view_mode: ViewMode::Overview,
            registers_state,
            threads_state,
            memory_regions_state,
            selected_thread_index: 0,
            error_message: None,
            info_message: None,
            info_message_time: None,
            process_output: VecDeque::new(),
            output_scrollback: 0,
            last_thread_refresh: std::time::Instant::now(),
            target_is_stopped: initial_is_stopped,
            last_stop_reason: initial_stop_reason,
            stop_event_log: VecDeque::new(),
            command_input: String::new(),
            command_palette_active: false,
            command_history: VecDeque::new(),
            command_history_index: None,
            cached_stack_trace: None,
            selected_frame_id: None,
            stack_frames_state,
            breakpoints_state,
            cached_breakpoints: Vec::new(),
            breakpoint_locations: std::collections::HashMap::new(),
            source_cache: std::collections::HashMap::new(),
            current_source_file: None,
            source_scroll: 0,
            timeline_log: VecDeque::new(),
            layout_preset: LayoutPreset::Standard,
            breakpoint_editor: None,
        };

        if initial_is_stopped {
            app.record_stop_event(format_stop_reason(initial_stop_reason));
            app.add_timeline_entry(TimelineEntryKind::Stop, format_stop_reason(initial_stop_reason));
        }

        // Initialize cached breakpoints
        app.refresh_breakpoints();

        app
    }

    /// Cleanup when quitting - detach from process
    ///
    /// This is an async function to avoid blocking the async runtime.
    /// It performs cleanup operations like killing launched processes and detaching.
    pub async fn cleanup(&mut self)
    {
        if self.debugger.is_attached() {
            // If we launched the process, kill it first before detaching
            // This ensures clean shutdown
            if self.was_launched
                && let Some(pid) = self.pid
            {
                // Try graceful shutdown first (non-blocking)
                let pid_str = pid.to_string();
                tokio::task::spawn_blocking(move || {
                    let _ = std::process::Command::new("kill").arg("-TERM").arg(&pid_str).output();
                })
                .await
                .ok();

                // Wait a bit for graceful shutdown (async)
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                // Force kill if still running (non-blocking)
                let pid_str = pid.to_string();
                tokio::task::spawn_blocking(move || {
                    let _ = std::process::Command::new("kill").arg("-9").arg(&pid_str).output();
                })
                .await
                .ok();
            }

            // Detach from the process
            if let Err(e) = self.debugger.detach() {
                eprintln!("Warning: Failed to detach from process: {e}");
            }
        }
    }

    /// Handle a keyboard event
    ///
    /// Returns `true` if the application should quit, `false` otherwise.
    #[allow(clippy::too_many_lines)]
    pub fn handle_key_event(&mut self, key_event: crossterm::event::KeyEvent) -> bool
    {
        use crossterm::event::{KeyCode, KeyModifiers};

        self.error_message = None;

        // Check for Ctrl+Q FIRST - this should always work to quit, regardless of mode
        if matches!(key_event.code, KeyCode::Char('q' | 'Q')) && key_event.modifiers.contains(KeyModifiers::CONTROL) {
            self.error_message = Some("Quitting...".to_string());
            self.should_quit = true;
            return true;
        }

        // Handle breakpoint editor input
        if self.breakpoint_editor.is_some() {
            return self.handle_breakpoint_editor_input(key_event);
        }

        // Handle command palette input
        if self.command_palette_active {
            return self.handle_command_palette_input(key_event);
        }

        match key_event.code {
            KeyCode::Char('q' | 'Q') => {
                // Regular 'q' without Ctrl - show help message
                self.error_message = Some("Press Esc to quit".to_string());
            }
            KeyCode::Esc => {
                if self.command_palette_active {
                    self.command_palette_active = false;
                    self.command_input.clear();
                } else if self.breakpoint_editor.is_some() {
                    self.breakpoint_editor = None;
                } else {
                    // Escape quits when not in any special mode
                    self.error_message = Some("Quitting...".to_string());
                    self.should_quit = true;
                    return true;
                }
            }
            KeyCode::Char('1') => {
                self.view_mode = ViewMode::Overview;
            }
            KeyCode::Char('2') => {
                self.view_mode = ViewMode::Registers;
            }
            KeyCode::Char('3') => {
                self.view_mode = ViewMode::Threads;
            }
            KeyCode::Char('4') => {
                self.view_mode = ViewMode::MemoryRegions;
            }
            KeyCode::Char('5') => {
                self.view_mode = ViewMode::Output;
            }
            KeyCode::Char('6') => {
                self.view_mode = ViewMode::Source;
                self.refresh_source_view();
            }
            KeyCode::Char('7') => {
                self.view_mode = ViewMode::Stack;
                self.refresh_stack_trace();
            }
            KeyCode::Char('8') => {
                self.view_mode = ViewMode::Timeline;
            }
            KeyCode::Char('9') => {
                self.view_mode = ViewMode::Help;
            }
            KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::Char('H') => {
                // Toggle help view
                if self.view_mode == ViewMode::Help {
                    // Return to previous view (default to Overview)
                    self.view_mode = ViewMode::Overview;
                } else {
                    self.view_mode = ViewMode::Help;
                }
            }
            KeyCode::Char(':') => {
                // Open command palette
                self.command_palette_active = true;
                self.command_input.clear();
            }
            KeyCode::Char('b') => {
                // Toggle breakpoint at current PC
                if self.debugger.is_attached()
                    && self.target_is_stopped
                    && let Ok(regs) = self.debugger.read_registers()
                {
                    let pc = regs.pc;
                    self.toggle_breakpoint_at_address(pc);
                }
            }
            KeyCode::Char('B') => {
                // Open breakpoint editor
                self.open_breakpoint_editor(None);
            }
            KeyCode::Char('s') => {
                if self.debugger.is_attached() {
                    if let Err(e) = self.debugger.suspend() {
                        self.error_message = Some(format!("Failed to suspend: {e}"));
                        self.info_message = None;
                    }
                } else {
                    self.error_message = Some("Not attached to a process".to_string());
                    self.info_message = None;
                }
            }
            KeyCode::Char('r') => {
                if self.debugger.is_attached() {
                    if let Err(e) = self.debugger.resume() {
                        self.error_message = Some(format!("Failed to resume: {e}"));
                        self.info_message = None;
                    }
                } else {
                    self.error_message = Some("Not attached to a process".to_string());
                    self.info_message = None;
                }
            }
            KeyCode::Char('n') => {
                // Next frame in stack view
                if self.view_mode == ViewMode::Stack {
                    self.navigate_stack_down();
                }
            }
            KeyCode::Char('p') => {
                // Previous frame in stack view
                if self.view_mode == ViewMode::Stack {
                    self.navigate_stack_up();
                }
            }
            KeyCode::Up => {
                self.navigate_up();
                // If in stack view, refresh source when navigating
                if self.view_mode == ViewMode::Stack {
                    self.refresh_source_view();
                }
            }
            KeyCode::Down => {
                self.navigate_down();
                // If in stack view, refresh source when navigating
                if self.view_mode == ViewMode::Stack {
                    self.refresh_source_view();
                }
            }
            KeyCode::Char('l') => {
                // Cycle layout presets
                self.cycle_layout_preset();
            }
            _ => {}
        }

        false
    }

    /// Navigate up in the current view
    fn navigate_up(&mut self)
    {
        match self.view_mode {
            ViewMode::Registers => {
                let i = self.registers_state.selected().unwrap_or(0);
                let max = self.get_register_count().saturating_sub(1);
                if max == 0 {
                    return;
                }
                let next = if i == 0 { max } else { i - 1 };
                self.registers_state.select(Some(next));
            }
            ViewMode::Threads => {
                let i = self.threads_state.selected().unwrap_or(0);
                if let Ok(threads) = self.debugger.threads() {
                    let max = threads.len().saturating_sub(1);
                    if max == 0 {
                        return;
                    }
                    let next = if i == 0 { max } else { i - 1 };
                    self.threads_state.select(Some(next));
                }
            }
            ViewMode::MemoryRegions => {
                let i = self.memory_regions_state.selected().unwrap_or(0);
                if let Ok(regions) = self.debugger.get_memory_regions() {
                    let max = regions.len().saturating_sub(1);
                    if max == 0 {
                        return;
                    }
                    let next = if i == 0 { max } else { i - 1 };
                    self.memory_regions_state.select(Some(next));
                }
            }
            ViewMode::Output => {
                self.scroll_output_up();
            }
            ViewMode::Source => {
                self.source_scroll += 1;
            }
            ViewMode::Stack => {
                self.navigate_stack_up();
            }
            ViewMode::Timeline | ViewMode::Overview | ViewMode::Help => {
                // Timeline auto-scrolls to bottom, no manual navigation needed
                // Help view doesn't support navigation
            }
        }
    }

    /// Navigate down in the current view
    fn navigate_down(&mut self)
    {
        match self.view_mode {
            ViewMode::Registers => {
                let i = self.registers_state.selected().unwrap_or(0);
                let max = self.get_register_count().saturating_sub(1);
                if max == 0 {
                    return;
                }
                let next = if i >= max { 0 } else { i + 1 };
                self.registers_state.select(Some(next));
            }
            ViewMode::Threads => {
                let i = self.threads_state.selected().unwrap_or(0);
                if let Ok(threads) = self.debugger.threads() {
                    let max = threads.len().saturating_sub(1);
                    if max == 0 {
                        return;
                    }
                    let next = if i >= max { 0 } else { i + 1 };
                    self.threads_state.select(Some(next));
                }
            }
            ViewMode::MemoryRegions => {
                let i = self.memory_regions_state.selected().unwrap_or(0);
                if let Ok(regions) = self.debugger.get_memory_regions() {
                    let max = regions.len().saturating_sub(1);
                    if max == 0 {
                        return;
                    }
                    let next = if i >= max { 0 } else { i + 1 };
                    self.memory_regions_state.select(Some(next));
                }
            }
            ViewMode::Output => {
                self.scroll_output_down();
            }
            ViewMode::Source => {
                if self.source_scroll > 0 {
                    self.source_scroll -= 1;
                }
            }
            ViewMode::Stack => {
                self.navigate_stack_down();
            }
            ViewMode::Timeline | ViewMode::Overview | ViewMode::Help => {
                // Timeline auto-scrolls to bottom, no manual navigation needed
                // Help view doesn't support navigation
            }
        }
    }

    /// Get the number of registers to display
    fn get_register_count(&self) -> usize
    {
        if let Ok(regs) = self.debugger.read_registers() {
            // Common registers (PC, SP, FP, Status) + general registers
            4 + regs.general.len()
        } else {
            0
        }
    }

    /// Update the application state (called on each tick)
    pub fn tick(&mut self)
    {
        // Refresh thread list periodically, but not too frequently
        // Mach thread ports are ephemeral and change on each refresh, so we only
        // refresh every 2 seconds to avoid showing constantly changing thread IDs
        if self.debugger.is_attached() {
            const THREAD_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
            if self.last_thread_refresh.elapsed() >= THREAD_REFRESH_INTERVAL {
                let _ = self.debugger.refresh_threads();
                self.last_thread_refresh = std::time::Instant::now();
            }

            // Refresh breakpoints periodically
            self.refresh_breakpoints();

            // Refresh stack trace if stopped (always, not just in stack view)
            // This ensures symbols are loaded even if user isn't viewing stack
            if self.target_is_stopped {
                self.refresh_stack_trace();
            }
        }

        // Auto-clear info messages after 3 seconds
        if let Some(time) = self.info_message_time {
            if time.elapsed().as_secs() >= 3 {
                self.info_message = None;
                self.info_message_time = None;
            }
        }
    }

    /// Consume an asynchronous debugger event from the core backend.
    pub fn handle_debugger_event(&mut self, event: &DebuggerEvent)
    {
        match event {
            DebuggerEvent::TargetStopped { reason, thread } => {
                self.target_is_stopped = true;
                self.last_stop_reason = *reason;
                let mut message = format_stop_reason(*reason);
                if let Some(thread_id) = thread {
                    let _ = write!(message, " (thread {})", thread_id.raw());
                }
                self.record_stop_event(message.clone());

                // Add to timeline
                let timeline_kind = match reason {
                    StopReason::Breakpoint(_) => TimelineEntryKind::BreakpointHit,
                    StopReason::Signal(_) => TimelineEntryKind::Signal,
                    _ => TimelineEntryKind::Stop,
                };
                self.add_timeline_entry(timeline_kind, message);

                // Refresh stack trace when stopped
                self.refresh_stack_trace();
                self.refresh_breakpoints();
            }
            DebuggerEvent::TargetResumed => {
                self.target_is_stopped = false;
                self.last_stop_reason = StopReason::Running;
                let message = "Target resumed execution".to_string();
                self.record_stop_event(message.clone());
                self.add_timeline_entry(TimelineEntryKind::Resume, message);
            }
        }
    }

    fn record_stop_event(&mut self, message: String)
    {
        self.stop_event_log.push_back(message);
        if self.stop_event_log.len() > MAX_STOP_EVENTS {
            self.stop_event_log.pop_front();
        }
    }

    /// User-facing summary of the current stop state.
    #[must_use]
    pub fn status_message(&self) -> String
    {
        if !self.debugger.is_attached() {
            return "Not attached to a process".to_string();
        }

        if self.target_is_stopped {
            format_stop_reason(self.last_stop_reason)
        } else {
            "Process is running".to_string()
        }
    }

    /// Append a captured process output line to the buffer.
    pub fn push_process_output(&mut self, source: ProcessOutputSource, line: &str)
    {
        let cleaned = line.trim_end_matches('\r').to_string();
        self.process_output.push_back(ProcessOutputLine { source, text: cleaned });
        if self.process_output.len() > MAX_PROCESS_OUTPUT_LINES {
            self.process_output.pop_front();
        }

        let max_scroll = self.process_output.len().saturating_sub(1);
        if self.output_scrollback > max_scroll {
            self.output_scrollback = max_scroll;
        }
    }

    fn scroll_output_up(&mut self)
    {
        if self.process_output.is_empty() {
            return;
        }
        let max_scroll = self.process_output.len().saturating_sub(1);
        if self.output_scrollback < max_scroll {
            self.output_scrollback += 1;
        }
    }

    fn scroll_output_down(&mut self)
    {
        if self.output_scrollback > 0 {
            self.output_scrollback -= 1;
        }
    }

    /// Refresh the cached stack trace
    pub fn refresh_stack_trace(&mut self)
    {
        if self.debugger.is_attached()
            && self.target_is_stopped
            && let Ok(frames) = self.debugger.stack_trace(64)
        {
            self.cached_stack_trace = Some(frames);
            if let Some(ref frames) = self.cached_stack_trace
                && !frames.is_empty()
            {
                self.selected_frame_id = Some(frames[0].id);
            }
        }
    }

    /// Refresh the cached breakpoints list
    pub fn refresh_breakpoints(&mut self)
    {
        self.cached_breakpoints = self.debugger.breakpoints();

        // Resolve breakpoint addresses to source locations for UI indicators
        // This allows us to show breakpoint markers in the source view
        // We use the stack trace frames which already have symbolication
        self.breakpoint_locations.clear();
        if let Some(ref frames) = self.cached_stack_trace {
            for bp in &self.cached_breakpoints {
                if bp.enabled && bp.state == ferros_core::BreakpointState::Resolved {
                    // Try to find a frame with matching PC to get source location
                    let location = frames
                        .iter()
                        .find(|frame| frame.pc == bp.address)
                        .and_then(|frame| frame.location.clone());
                    self.breakpoint_locations.insert(bp.address, location);
                }
            }
        }
    }

    /// Refresh the source view based on current frame
    pub fn refresh_source_view(&mut self)
    {
        // Use the selected frame from stack view, or fall back to first frame
        let selected_idx = self.stack_frames_state.selected().unwrap_or(0);
        if let Some(ref frames) = self.cached_stack_trace
            && let Some(frame) = frames.get(selected_idx)
            && let Some(ref location) = frame.location
        {
            if !self.source_cache.contains_key(&location.file) {
                // Try to load the source file
                if let Ok(content) = std::fs::read_to_string(&location.file) {
                    let lines: Vec<String> = content.lines().map(str::to_string).collect();
                    self.source_cache.insert(location.file.clone(), lines);
                }
            }
            self.current_source_file = Some(location.file.clone());
            if let Some(line) = location.line {
                self.source_scroll = (line as usize).saturating_sub(10).max(0);
            }
        }
    }

    /// Navigate up in stack view
    fn navigate_stack_up(&mut self)
    {
        if let Some(ref frames) = self.cached_stack_trace {
            let current_idx = self.stack_frames_state.selected().unwrap_or(0);
            if current_idx > 0 {
                self.stack_frames_state.select(Some(current_idx - 1));
                if let Some(frame) = frames.get(current_idx - 1) {
                    self.selected_frame_id = Some(frame.id);
                }
                // Refresh source view when frame selection changes
                self.refresh_source_view();
            }
        }
    }

    /// Navigate down in stack view
    fn navigate_stack_down(&mut self)
    {
        if let Some(ref frames) = self.cached_stack_trace {
            let current_idx = self.stack_frames_state.selected().unwrap_or(0);
            let max = frames.len().saturating_sub(1);
            if current_idx < max {
                self.stack_frames_state.select(Some(current_idx + 1));
                if let Some(frame) = frames.get(current_idx + 1) {
                    self.selected_frame_id = Some(frame.id);
                }
                // Refresh source view when frame selection changes
                self.refresh_source_view();
            }
        }
    }

    /// Toggle breakpoint at the given address
    fn toggle_breakpoint_at_address(&mut self, address: Address)
    {
        // Check if breakpoint already exists
        let existing_bp = self.cached_breakpoints.iter().find(|bp| bp.address == address).cloned();

        if let Some(bp) = existing_bp {
            // Toggle existing breakpoint
            let was_enabled = bp.enabled;
            if let Err(e) = self.debugger.toggle_breakpoint(bp.id) {
                self.error_message = Some(format!("Failed to toggle breakpoint: {e}"));
                self.info_message = None;
            } else {
                self.refresh_breakpoints();
                let action = if was_enabled { "Disabled" } else { "Enabled" };
                let message = format!("{} breakpoint #{} at {}", action, bp.id.raw(), address);
                self.info_message = Some(message.clone());
                self.info_message_time = Some(std::time::Instant::now());
                self.error_message = None;
                self.add_timeline_entry(TimelineEntryKind::BreakpointHit, format!("Toggled breakpoint at {address}"));
            }
        } else {
            // Add new breakpoint
            if let Err(e) = self
                .debugger
                .add_breakpoint(ferros_core::BreakpointRequest::Software { address })
            {
                self.error_message = Some(format!("Failed to add breakpoint: {e}"));
                self.info_message = None;
            } else {
                self.refresh_breakpoints();
                // Find the newly created breakpoint to show its type and ID
                let new_bp = self.cached_breakpoints.iter().find(|bp| bp.address == address).cloned();

                if let Some(bp) = new_bp {
                    let kind_str = match bp.kind {
                        ferros_core::BreakpointKind::Software => "software",
                        ferros_core::BreakpointKind::Hardware => "hardware",
                        ferros_core::BreakpointKind::Watchpoint => "watchpoint",
                    };
                    let message = format!("Added {} breakpoint #{} at {}", kind_str, bp.id.raw(), address);
                    self.info_message = Some(message.clone());
                    self.info_message_time = Some(std::time::Instant::now());
                    self.error_message = None;
                    self.add_timeline_entry(TimelineEntryKind::BreakpointHit, message);
                }
            }
        }
    }

    /// Open breakpoint editor
    fn open_breakpoint_editor(&mut self, existing_id: Option<BreakpointId>)
    {
        if let Some(id) = existing_id {
            if let Ok(info) = self.debugger.breakpoint_info(id) {
                self.breakpoint_editor = Some(BreakpointEditorState {
                    address_input: format!("{}", info.address),
                    kind_input: match info.kind {
                        ferros_core::BreakpointKind::Software => "software".to_string(),
                        ferros_core::BreakpointKind::Hardware => "hardware".to_string(),
                        ferros_core::BreakpointKind::Watchpoint => "watchpoint".to_string(),
                    },
                    watch_length_input: info.watch_length.map(|l| l.to_string()).unwrap_or_default(),
                    watch_access_input: info
                        .watch_access
                        .map(|a| match a {
                            ferros_core::WatchpointAccess::Read => "read".to_string(),
                            ferros_core::WatchpointAccess::Write => "write".to_string(),
                            ferros_core::WatchpointAccess::ReadWrite => "readwrite".to_string(),
                        })
                        .unwrap_or_default(),
                    editing_existing: Some(id),
                });
            }
        } else {
            self.breakpoint_editor = Some(BreakpointEditorState {
                address_input: String::new(),
                kind_input: "software".to_string(),
                watch_length_input: String::new(),
                watch_access_input: String::new(),
                editing_existing: None,
            });
        }
    }

    /// Handle input in breakpoint editor
    fn handle_breakpoint_editor_input(&mut self, key_event: crossterm::event::KeyEvent) -> bool
    {
        use crossterm::event::{KeyCode, KeyModifiers};

        if let Some(ref mut editor) = self.breakpoint_editor {
            match key_event.code {
                KeyCode::Enter => {
                    // Apply breakpoint
                    self.apply_breakpoint_editor();
                    return false;
                }
                KeyCode::Esc => {
                    self.breakpoint_editor = None;
                    return false;
                }
                KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    editor.address_input.push(c);
                }
                KeyCode::Backspace => {
                    editor.address_input.pop();
                }
                _ => {}
            }
        }
        false
    }

    /// Apply breakpoint editor changes
    fn apply_breakpoint_editor(&mut self)
    {
        if let Some(editor) = self.breakpoint_editor.take() {
            // Parse address
            if let Ok(addr_value) = u64::from_str_radix(editor.address_input.trim_start_matches("0x"), 16) {
                let address = Address::from(addr_value);

                let request = match editor.kind_input.as_str() {
                    "hardware" => ferros_core::BreakpointRequest::Hardware { address },
                    "watchpoint" => {
                        let length = editor.watch_length_input.parse().unwrap_or(8);
                        let access = match editor.watch_access_input.as_str() {
                            "read" => ferros_core::WatchpointAccess::Read,
                            "write" => ferros_core::WatchpointAccess::Write,
                            _ => ferros_core::WatchpointAccess::ReadWrite,
                        };
                        ferros_core::BreakpointRequest::Watchpoint { address, length, access }
                    }
                    _ => ferros_core::BreakpointRequest::Software { address },
                };

                if let Some(existing_id) = editor.editing_existing {
                    // Remove old and add new
                    let _ = self.debugger.remove_breakpoint(existing_id);
                }

                if let Err(e) = self.debugger.add_breakpoint(request) {
                    self.error_message = Some(format!("Failed to add breakpoint: {e}"));
                } else {
                    self.refresh_breakpoints();
                    self.add_timeline_entry(TimelineEntryKind::BreakpointHit, format!("Breakpoint added at {address}"));
                }
            } else {
                self.error_message = Some("Invalid address format".to_string());
            }
        }
    }

    /// Handle input in command palette
    fn handle_command_palette_input(&mut self, key_event: crossterm::event::KeyEvent) -> bool
    {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key_event.code {
            KeyCode::Enter => {
                self.execute_command();
                return false;
            }
            KeyCode::Esc => {
                self.command_palette_active = false;
                self.command_input.clear();
                return false;
            }
            KeyCode::Up => {
                // Navigate command history
                if let Some(ref mut idx) = self.command_history_index {
                    if *idx > 0 {
                        *idx -= 1;
                        if let Some(cmd) = self.command_history.get(*idx) {
                            self.command_input = cmd.clone();
                        }
                    }
                } else if !self.command_history.is_empty() {
                    self.command_history_index = Some(self.command_history.len() - 1);
                    if let Some(cmd) = self.command_history.back() {
                        self.command_input = cmd.clone();
                    }
                }
            }
            KeyCode::Down => {
                // Navigate command history forward
                if let Some(ref mut idx) = self.command_history_index {
                    if *idx < self.command_history.len().saturating_sub(1) {
                        *idx += 1;
                        if let Some(cmd) = self.command_history.get(*idx) {
                            self.command_input = cmd.clone();
                        }
                    } else {
                        self.command_history_index = None;
                        self.command_input.clear();
                    }
                }
            }
            KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.command_input.push(c);
                self.command_history_index = None;
            }
            KeyCode::Backspace => {
                self.command_input.pop();
                self.command_history_index = None;
            }
            _ => {}
        }
        false
    }

    /// Execute a command from the command palette
    #[allow(clippy::too_many_lines)]
    fn execute_command(&mut self)
    {
        let cmd = self.command_input.trim();
        if cmd.is_empty() {
            self.command_palette_active = false;
            return;
        }

        // Add to history
        if self.command_history.back().map(String::as_str) != Some(cmd) {
            self.command_history.push_back(cmd.to_string());
            if self.command_history.len() > 100 {
                self.command_history.pop_front();
            }
        }

        // Parse and execute command
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.is_empty() {
            self.command_palette_active = false;
            return;
        }

        match parts[0] {
            "break" | "b" => {
                if parts.len() > 1
                    && let Ok(addr) = u64::from_str_radix(parts[1].trim_start_matches("0x"), 16)
                {
                    let address = Address::from(addr);
                    if let Err(e) = self
                        .debugger
                        .add_breakpoint(ferros_core::BreakpointRequest::Software { address })
                    {
                        self.error_message = Some(format!("Failed to add breakpoint: {e}"));
                    } else {
                        self.refresh_breakpoints();
                        self.add_timeline_entry(TimelineEntryKind::BreakpointHit, format!("Breakpoint at {address}"));
                    }
                } else if parts.len() > 1 {
                    self.error_message = Some("Invalid address format".to_string());
                }
            }
            "delete" | "d" => {
                if parts.len() > 1
                    && let Ok(id) = parts[1].parse::<u64>()
                {
                    let bp_id = ferros_core::BreakpointId::from_raw(id);
                    if let Err(e) = self.debugger.remove_breakpoint(bp_id) {
                        self.error_message = Some(format!("Failed to remove breakpoint: {e}"));
                    } else {
                        self.refresh_breakpoints();
                        self.add_timeline_entry(TimelineEntryKind::BreakpointHit, format!("Removed breakpoint {id}"));
                    }
                }
            }
            "enable" | "e" => {
                if parts.len() > 1
                    && let Ok(id) = parts[1].parse::<u64>()
                {
                    let bp_id = ferros_core::BreakpointId::from_raw(id);
                    if let Err(e) = self.debugger.enable_breakpoint(bp_id) {
                        self.error_message = Some(format!("Failed to enable breakpoint: {e}"));
                    } else {
                        self.refresh_breakpoints();
                    }
                }
            }
            "disable" => {
                if parts.len() > 1
                    && let Ok(id) = parts[1].parse::<u64>()
                {
                    let bp_id = ferros_core::BreakpointId::from_raw(id);
                    if let Err(e) = self.debugger.disable_breakpoint(bp_id) {
                        self.error_message = Some(format!("Failed to disable breakpoint: {e}"));
                    } else {
                        self.refresh_breakpoints();
                    }
                }
            }
            "help" | "h" => {
                // Toggle help view
                if self.view_mode == ViewMode::Help {
                    self.view_mode = ViewMode::Overview;
                } else {
                    self.view_mode = ViewMode::Help;
                }
            }
            "frame" | "f" => {
                if parts.len() > 1
                    && let Ok(idx) = parts[1].parse::<usize>()
                    && let Some(ref frames) = self.cached_stack_trace
                    && idx < frames.len()
                {
                    self.stack_frames_state.select(Some(idx));
                    self.selected_frame_id = Some(frames[idx].id);
                    self.view_mode = ViewMode::Stack;
                }
            }
            "thread" | "t" => {
                if parts.len() > 1
                    && let Ok(thread_id) = parts[1].parse::<u64>()
                {
                    let tid = ThreadId::from(thread_id);
                    if let Err(e) = self.debugger.set_active_thread(tid) {
                        self.error_message = Some(format!("Failed to set active thread: {e}"));
                    } else {
                        self.refresh_stack_trace();
                    }
                }
            }
            _ => {
                self.error_message = Some(format!("Unknown command: {cmd}. Type 'help' for commands."));
            }
        }

        self.command_palette_active = false;
        self.command_input.clear();
    }

    /// Cycle through layout presets
    fn cycle_layout_preset(&mut self)
    {
        self.layout_preset = match self.layout_preset {
            LayoutPreset::Compact => LayoutPreset::Standard,
            LayoutPreset::Standard => LayoutPreset::Widescreen,
            LayoutPreset::Widescreen => LayoutPreset::Compact,
        };
    }

    /// Add an entry to the timeline log
    pub fn add_timeline_entry(&mut self, kind: TimelineEntryKind, message: String)
    {
        self.timeline_log.push_back(TimelineEntry {
            timestamp: std::time::Instant::now(),
            kind,
            message,
        });
        if self.timeline_log.len() > MAX_TIMELINE_ENTRIES {
            self.timeline_log.pop_front();
        }
    }
}
