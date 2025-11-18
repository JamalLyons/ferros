//! Application state and logic

use std::collections::VecDeque;

use ferros_core::Debugger;
use ratatui::widgets::TableState;

/// Maximum number of process output lines retained in memory.
const MAX_PROCESS_OUTPUT_LINES: usize = 4096;

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
    /// Process output buffer (for displaying in TUI)
    pub process_output: VecDeque<ProcessOutputLine>,
    /// Number of lines scrolled back from the end of the output buffer
    pub output_scrollback: usize,
    /// Timestamp of last thread list refresh (to avoid refreshing too frequently)
    last_thread_refresh: std::time::Instant,
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
}

impl App
{
    /// Create a new application instance
    #[must_use]
    pub fn new(debugger: Box<dyn Debugger>, pid: Option<u32>, was_launched: bool) -> Self
    {
        let mut registers_state = TableState::default();
        registers_state.select(Some(0));

        let mut threads_state = TableState::default();
        threads_state.select(Some(0));

        let mut memory_regions_state = TableState::default();
        memory_regions_state.select(Some(0));

        Self {
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
            process_output: VecDeque::new(),
            output_scrollback: 0,
            last_thread_refresh: std::time::Instant::now(),
        }
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
            if self.was_launched {
                if let Some(pid) = self.pid {
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
    pub fn handle_key_event(&mut self, key_event: crossterm::event::KeyEvent) -> bool
    {
        use crossterm::event::{KeyCode, KeyModifiers};

        self.error_message = None;

        match key_event.code {
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => {
                self.error_message = Some("Quitting...".to_string());
                self.should_quit = true;
                return true;
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
            KeyCode::Char('s') => {
                if self.debugger.is_attached() {
                    if let Err(e) = self.debugger.suspend() {
                        self.error_message = Some(format!("Failed to suspend: {e}"));
                    }
                } else {
                    self.error_message = Some("Not attached to a process".to_string());
                }
            }
            KeyCode::Char('r') => {
                if self.debugger.is_attached() {
                    if let Err(e) = self.debugger.resume() {
                        self.error_message = Some(format!("Failed to resume: {e}"));
                    }
                } else {
                    self.error_message = Some("Not attached to a process".to_string());
                }
            }
            KeyCode::Up => {
                self.navigate_up();
            }
            KeyCode::Down => {
                self.navigate_down();
            }
            KeyCode::Char('a') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+A: Attach to process (prompt for PID)
                // For now, just show an error - in a real implementation,
                // you'd want a prompt dialog
                self.error_message = Some("Use 'ferros attach <pid>' from CLI to attach".to_string());
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
            ViewMode::Overview => {}
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
            ViewMode::Overview => {}
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
}
