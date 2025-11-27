//! Widget components for displaying debugger information

use ferros_core::events::format_stop_reason;
use ferros_core::types::Architecture;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::app::{App, ProcessOutputLine, ProcessOutputSource};

/// Check if a register value looks like a valid memory address
///
/// This is a heuristic that checks if a u64 value could be a pointer/address.
/// On 64-bit systems, valid user-space addresses are typically:
/// - Non-zero
/// - Within reasonable memory ranges (not too small, not too large)
/// - Often aligned (though not always)
fn looks_like_address(value: u64) -> bool
{
    // Zero is not a valid address (null pointer)
    if value == 0 {
        return false;
    }

    // On 64-bit systems, addresses are typically in certain ranges
    // macOS ARM64 user-space addresses are typically:
    // - Stack: 0x000000016... to 0x000000017...
    // - Heap: 0x000000020... to 0x000000040...
    // - Code: 0x000000010... to 0x000000020...
    // - Mapped: Various ranges

    // Very small values (< 0x1000) are likely not addresses
    if value < 0x1000 {
        return false;
    }

    // Very large values (> 0x7fff_ffff_ffff) are likely not valid user-space addresses
    // (sign bit would be set, or beyond typical address space)
    if value > 0x7fff_ffff_ffff {
        return false;
    }

    // If it's in a reasonable range, it could be an address
    true
}

/// Format a memory size in bytes to a human-readable string (KB, MB, or GB)
#[allow(clippy::large_stack_arrays)]
fn format_memory_size(size_bytes: u64) -> String
{
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if size_bytes >= GB {
        let whole = size_bytes / GB;
        let remainder = size_bytes % GB;
        let fraction = (remainder * 100) / GB;
        format!("{whole}.{fraction:02} GB")
    } else if size_bytes >= MB {
        let whole = size_bytes / MB;
        let remainder = size_bytes % MB;
        let fraction = (remainder * 100) / MB;
        format!("{whole}.{fraction:02} MB")
    } else if size_bytes >= KB {
        let whole = size_bytes / KB;
        let remainder = size_bytes % KB;
        let fraction = (remainder * 100) / KB;
        format!("{whole}.{fraction:02} KB")
    } else {
        format!("{size_bytes} B")
    }
}

/// Draw the overview screen
pub fn draw_overview(frame: &mut Frame, area: Rect, app: &App)
{
    // Use boxed slice to avoid large stack array warning
    let constraints: Box<[Constraint]> = Box::new([
        Constraint::Length(10), // Debugger info
        Constraint::Min(0),     // Status
    ]);
    let chunks = Layout::vertical(constraints).split(area);

    draw_debugger_info(frame, chunks[0], app);
    draw_status(frame, chunks[1], app);
}

/// Draw debugger information
fn draw_debugger_info(frame: &mut Frame, area: Rect, app: &App)
{
    let info_lines = vec![
        Line::from(vec![
            Span::styled("Architecture: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{}", app.debugger.architecture())),
        ]),
        Line::from(vec![
            Span::styled("Attached: ", Style::default().fg(Color::Yellow)),
            Span::raw(if app.debugger.is_attached() { "Yes" } else { "No" }),
        ]),
        Line::from(vec![
            Span::styled("Stopped: ", Style::default().fg(Color::Yellow)),
            Span::raw(if app.target_is_stopped { "Yes" } else { "No" }),
        ]),
        Line::from(vec![
            Span::styled("Stop Reason: ", Style::default().fg(Color::Yellow)),
            Span::raw(if app.target_is_stopped {
                format_stop_reason(app.last_stop_reason)
            } else {
                "N/A".to_string()
            }),
        ]),
    ];

    let mut lines = info_lines;

    if app.debugger.is_attached() {
        if let Ok(threads) = app.debugger.threads() {
            lines.push(Line::from(vec![
                Span::styled("Threads: ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{}", threads.len())),
            ]));

            if let Some(active) = app.debugger.active_thread() {
                lines.push(Line::from(vec![
                    Span::styled("Active Thread: ", Style::default().fg(Color::Yellow)),
                    Span::raw(format!("{}", active.raw())),
                ]));
            }
        }

        if let Ok(regions) = app.debugger.get_memory_regions() {
            lines.push(Line::from(vec![
                Span::styled("Memory Regions: ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{}", regions.len())),
            ]));
        }
    }

    let info = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Debugger Information"))
        .style(Style::default().fg(Color::White));

    frame.render_widget(info, area);
}

/// Draw status information
fn draw_status(frame: &mut Frame, area: Rect, app: &App)
{
    let mut lines = vec![Line::from(app.status_message())];

    if let Some(latest) = app.stop_event_log.back() {
        lines.push(Line::from(format!("Last event: {latest}")));
    }

    let status = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .style(Style::default().fg(Color::Green));

    frame.render_widget(status, area);
}

/// Draw the registers view
pub fn draw_registers(frame: &mut Frame, area: Rect, app: &mut App)
{
    let registers = match app.debugger.read_registers() {
        Ok(regs) => regs,
        Err(e) => {
            let error = Paragraph::new(format!("Error reading registers: {e}"))
                .block(Block::default().borders(Borders::ALL).title("Registers"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return;
        }
    };

    let arch = registers.architecture();
    let mut rows = Vec::new();

    // Common registers
    rows.push(Row::new(vec![
        Cell::from("PC"),
        Cell::from(format!("{}", registers.pc)),
        Cell::from(format!("0x{:016x}", registers.pc.value())),
    ]));
    rows.push(Row::new(vec![
        Cell::from("SP"),
        Cell::from(format!("{}", registers.sp)),
        Cell::from(format!("0x{:016x}", registers.sp.value())),
    ]));
    rows.push(Row::new(vec![
        Cell::from("FP"),
        Cell::from(format!("{}", registers.fp)),
        Cell::from(format!("0x{:016x}", registers.fp.value())),
    ]));
    rows.push(Row::new(vec![
        Cell::from("Status"),
        Cell::from(format!("0x{:016x}", registers.status)),
        Cell::from(""),
    ]));

    // Architecture-specific registers
    match arch {
        Architecture::Arm64 => {
            for (i, val) in registers.general.iter().enumerate() {
                let address_cell = if looks_like_address(*val) {
                    format!("0x{val:016x}")
                } else {
                    String::new()
                };
                rows.push(Row::new(vec![
                    Cell::from(format!("X{i}")),
                    Cell::from(format!("0x{val:016x}")),
                    Cell::from(address_cell),
                ]));
            }
        }
        Architecture::X86_64 => {
            // Use boxed slice to avoid large stack array warning
            let reg_names: Box<[&str]> = vec![
                "RAX", "RBX", "RCX", "RDX", "RSI", "RDI", "R8", "R9", "R10", "R11", "R12", "R13", "R14", "R15",
            ]
            .into_boxed_slice();
            for (i, val) in registers.general.iter().enumerate() {
                if i < reg_names.len() {
                    let address_cell = if looks_like_address(*val) {
                        format!("0x{val:016x}")
                    } else {
                        String::new()
                    };
                    rows.push(Row::new(vec![
                        Cell::from(reg_names[i]),
                        Cell::from(format!("0x{val:016x}")),
                        Cell::from(address_cell),
                    ]));
                }
            }
        }
        Architecture::Unknown(_) => {
            for (i, val) in registers.general.iter().enumerate() {
                let address_cell = if looks_like_address(*val) {
                    format!("0x{val:016x}")
                } else {
                    String::new()
                };
                rows.push(Row::new(vec![
                    Cell::from(format!("R{i}")),
                    Cell::from(format!("0x{val:016x}")),
                    Cell::from(address_cell),
                ]));
            }
        }
    }

    // Use boxed slice to avoid large stack array warning
    let constraints: Box<[Constraint]> = Box::new([Constraint::Length(10), Constraint::Length(20), Constraint::Length(20)]);
    let table = Table::new(rows, constraints)
        .block(Block::default().borders(Borders::ALL).title("Registers"))
        .header(Row::new(vec![
            Cell::from("Register").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Value (hex)").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Address").style(Style::default().add_modifier(Modifier::BOLD)),
        ]))
        .row_highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(table, area, &mut app.registers_state);
}

/// Draw the threads view
pub fn draw_threads(frame: &mut Frame, area: Rect, app: &mut App)
{
    let threads = match app.debugger.threads() {
        Ok(threads) => threads,
        Err(e) => {
            let error = Paragraph::new(format!("Error reading threads: {e}"))
                .block(Block::default().borders(Borders::ALL).title("Threads"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return;
        }
    };

    let active_thread = app.debugger.active_thread();

    let rows: Vec<Row> = threads
        .iter()
        .enumerate()
        .map(|(i, thread)| {
            let is_active = active_thread.is_some_and(|t| t == *thread);
            let prefix = if is_active { "→ " } else { "  " };
            Row::new(vec![
                Cell::from(format!("{prefix}{i}")),
                Cell::from(format!("{}", thread.raw())),
                Cell::from(if is_active { "Active" } else { "" }),
            ])
        })
        .collect();

    // Use boxed slice to avoid large stack array warning
    let constraints: Box<[Constraint]> = Box::new([Constraint::Length(10), Constraint::Length(20), Constraint::Length(10)]);
    let table = Table::new(rows, constraints)
        .block(Block::default().borders(Borders::ALL).title("Threads"))
        .header(Row::new(vec![
            Cell::from("Index").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Thread ID").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Status").style(Style::default().add_modifier(Modifier::BOLD)),
        ]))
        .row_highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(table, area, &mut app.threads_state);
}

/// Draw the memory regions view
pub fn draw_memory_regions(frame: &mut Frame, area: Rect, app: &mut App)
{
    let regions = match app.debugger.get_memory_regions() {
        Ok(regions) => regions,
        Err(e) => {
            let error = Paragraph::new(format!("Error reading memory regions: {e}"))
                .block(Block::default().borders(Borders::ALL).title("Memory Regions"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return;
        }
    };

    let rows: Vec<Row> = regions
        .iter()
        .map(|region| {
            let size_str = format_memory_size(region.size());
            Row::new(vec![
                Cell::from(format!("{}", region.id.value())),
                Cell::from(format!("{}", region.start)),
                Cell::from(format!("{}", region.end)),
                Cell::from(size_str),
                Cell::from(region.permissions.clone()),
                Cell::from(region.name.as_deref().unwrap_or("").to_string()),
            ])
        })
        .collect();

    // Use boxed slice to avoid large stack array warning
    let constraints: Box<[Constraint]> = vec![
        Constraint::Length(5),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(12),
        Constraint::Length(6),
        Constraint::Min(0),
    ]
    .into_boxed_slice();
    let table = Table::new(rows, constraints)
        .block(Block::default().borders(Borders::ALL).title("Memory Regions"))
        .header(Row::new(vec![
            Cell::from("ID").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Start").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("End").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Size").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Perms").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Name").style(Style::default().add_modifier(Modifier::BOLD)),
        ]))
        .row_highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(table, area, &mut app.memory_regions_state);
}

/// Draw the process output view
pub fn draw_output(frame: &mut Frame, area: Rect, app: &App)
{
    let viewport_height = area.height.saturating_sub(2) as usize; // account for borders
    let mut output_text = Vec::new();

    if app.process_output.is_empty() {
        output_text.extend(vec![
            Line::from("No process output captured yet."),
            Line::from(""),
            Line::from("Output is captured automatically when launching a new target from Ferros."),
            Line::from("Attach mode reuses the target's existing stdout/stderr."),
        ]);
    } else {
        let visible_lines = viewport_height.max(1);
        let total_lines = app.process_output.len();
        let scrollback = app.output_scrollback.min(total_lines.saturating_sub(1));
        let start_index = total_lines.saturating_sub(visible_lines).saturating_sub(scrollback);
        let lines_to_show = visible_lines.min(total_lines.saturating_sub(start_index));

        output_text.extend(
            app.process_output
                .iter()
                .skip(start_index)
                .take(lines_to_show)
                .map(format_process_output_line),
        );
    }

    let output = Paragraph::new(output_text)
        .block(Block::default().borders(Borders::ALL).title("Process Output"))
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: true });

    frame.render_widget(output, area);
}

fn format_process_output_line(entry: &ProcessOutputLine) -> Line<'_>
{
    let (label, color) = match entry.source {
        ProcessOutputSource::Stdout => ("stdout", Color::Green),
        ProcessOutputSource::Stderr => ("stderr", Color::Red),
    };

    Line::from(vec![
        Span::styled(format!("[{label}]"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::raw(entry.text.clone()),
    ])
}

/// Draw the command palette
pub fn draw_command_palette(frame: &mut Frame, area: Rect, app: &App)
{
    let center_y = area.height / 2;
    let center_x = area.width / 2;
    let width = area.width.min(80);
    let height = 3;

    let palette_area = Rect {
        x: center_x.saturating_sub(width / 2),
        y: center_y.saturating_sub(height / 2),
        width,
        height,
    };

    let input_text = format!(":{}", app.command_input);
    let input = Paragraph::new(input_text.as_str())
        .block(Block::default().borders(Borders::ALL).title("Command"))
        .style(Style::default().fg(Color::Yellow));

    frame.render_widget(input, palette_area);

    // Set cursor position for input
    let cursor_offset = app.command_input.len().min(width as usize - 2);
    let cursor_offset = u16::try_from(cursor_offset).unwrap_or(u16::MAX);
    frame.set_cursor_position((palette_area.x + 1 + cursor_offset, palette_area.y + 1));
}

/// Draw the breakpoint editor
pub fn draw_breakpoint_editor(frame: &mut Frame, area: Rect, app: &App)
{
    let center_y = area.height / 2;
    let center_x = area.width / 2;
    let width = area.width.min(60);
    let height = 8;

    let editor_area = Rect {
        x: center_x.saturating_sub(width / 2),
        y: center_y.saturating_sub(height / 2),
        width,
        height,
    };

    if let Some(ref editor) = app.breakpoint_editor {
        let lines = vec![
            Line::from("Breakpoint Editor"),
            Line::from(""),
            Line::from(vec![
                Span::raw("Address: "),
                Span::styled(&editor.address_input, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                Span::raw("Kind: "),
                Span::styled(&editor.kind_input, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(""),
            Line::from("Press Enter to apply, Esc to cancel"),
        ];

        let editor_widget = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Breakpoint Editor"))
            .style(Style::default().fg(Color::White));

        frame.render_widget(editor_widget, editor_area);
    }
}

/// Draw the source code view with breakpoints
pub fn draw_source_view(frame: &mut Frame, area: Rect, app: &mut App)
{
    // Split into source (left) and breakpoints (right) if widescreen
    let constraints: Box<[Constraint]> = match app.layout_preset {
        crate::app::LayoutPreset::Compact | crate::app::LayoutPreset::Standard => Box::new([Constraint::Percentage(100)]),
        crate::app::LayoutPreset::Widescreen => Box::new([Constraint::Percentage(70), Constraint::Percentage(30)]),
    };

    let chunks = Layout::horizontal(constraints).split(area);

    draw_source_code(frame, chunks[0], app);

    if chunks.len() > 1 {
        draw_breakpoints_list(frame, chunks[1], app);
    }
}

/// Draw source code with breakpoint gutter
fn draw_source_code(frame: &mut Frame, area: Rect, app: &mut App)
{
    // Get current PC to highlight (may be used for future features)
    let _current_pc = if app.debugger.is_attached() && app.target_is_stopped {
        app.debugger.read_registers().ok().map(|r| r.pc)
    } else {
        None
    };

    // Get source file - prefer current_source_file, otherwise try to find from frames
    let source_file = app.current_source_file.clone().or_else(|| {
        if let Some(ref frames) = app.cached_stack_trace {
            let selected_idx = app.stack_frames_state.selected().unwrap_or(0);
            if let Some(frame) = frames.get(selected_idx) {
                frame.location.as_ref().map(|loc| loc.file.clone())
            } else if let Some(frame) = frames.first() {
                frame.location.as_ref().map(|loc| loc.file.clone())
            } else {
                None
            }
        } else {
            None
        }
    });

    if let Some(ref file) = source_file {
        if let Some(lines) = app.source_cache.get(file) {
            let viewport_height = area.height.saturating_sub(2) as usize;
            let start_line = app.source_scroll.min(lines.len().saturating_sub(1));
            let end_line = (start_line + viewport_height).min(lines.len());

            let mut source_lines = Vec::new();
            for (i, line) in lines.iter().enumerate().skip(start_line).take(end_line - start_line) {
                let line_num = i + 1;
                let line_num_str = format!("{line_num:4} ");

                // Check for breakpoint at this line by matching source location
                let line_u32 = u32::try_from(line_num).unwrap_or(u32::MAX);
                let has_breakpoint = app.breakpoint_locations.iter().any(|(_addr, location_opt)| {
                    location_opt.as_ref().is_some_and(|location| {
                        location.file == *file && location.line == Some(line_u32)
                    })
                });

                // Check if this is the current line (use selected frame from stack view)
                let is_current = if let Some(ref frames) = app.cached_stack_trace {
                    let selected_idx = app.stack_frames_state.selected().unwrap_or(0);
                    if let Some(frame) = frames.get(selected_idx)
                        && let Some(ref location) = frame.location
                    {
                        let line_u32 = u32::try_from(line_num).unwrap_or(u32::MAX);
                        location.file == *file && location.line == Some(line_u32)
                    } else {
                        false
                    }
                } else {
                    false
                };
                
                // Check if this line is selected (for setting breakpoints)
                let is_selected = app.source_selected_line
                    .map(|selected| selected == i)
                    .unwrap_or(false);

                let mut spans = vec![Span::styled(line_num_str, Style::default().fg(Color::DarkGray))];

                if has_breakpoint {
                    spans.push(Span::styled("● ", Style::default().fg(Color::Red)));
                } else {
                    spans.push(Span::raw("  "));
                }

                let line_style = if is_current {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                        .bg(Color::DarkGray)
                } else if is_selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default().fg(Color::White)
                };

                spans.push(Span::styled(line.clone(), line_style));
                source_lines.push(Line::from(spans));
            }

            let source_widget = Paragraph::new(source_lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Source: {}", file.split('/').next_back().unwrap_or(file))),
                )
                .style(Style::default().fg(Color::White))
                .wrap(ratatui::widgets::Wrap { trim: false });

            frame.render_widget(source_widget, area);
        } else {
            let error = Paragraph::new("No source code available")
                .block(Block::default().borders(Borders::ALL).title("Source"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
        }
    } else {
        // Provide helpful error message based on why source isn't loaded
        let error_msg = if !app.debugger.is_attached() {
            "Not attached to a process. Launch or attach to a process first."
        } else if !app.target_is_stopped {
            "Process is running. Suspend the process (press 's') or wait for a breakpoint to view source."
        } else if app.cached_stack_trace.is_none() {
            "No stack trace available. The process may not have debug symbols."
        } else if app.cached_stack_trace.as_ref().is_some_and(|frames| frames.is_empty()) {
            "Stack trace is empty. No frames available."
        } else {
            // Check if we have frames but they don't have source locations
            let has_frames_without_source = app.cached_stack_trace.as_ref().is_some_and(|frames| {
                frames.iter().all(|f| f.location.is_none())
            });
            
            if has_frames_without_source {
                "Stack frames exist but none have source location information.\nThis usually means:\n  • Program wasn't built with debug symbols\n  • Source files aren't available at the paths in DWARF\n\nTry building with: cargo build --example test_target"
            } else {
                "No source file available for current frame. Try:\n  1. Navigate to Stack view (press '7')\n  2. Select a frame with source info (↑/↓)\n  3. Return to Source view (press '6')"
            }
        };
        
        let error = Paragraph::new(error_msg)
            .block(Block::default().borders(Borders::ALL).title("Source"))
            .style(Style::default().fg(Color::Yellow))
            .wrap(ratatui::widgets::Wrap { trim: false });
        frame.render_widget(error, area);
    }
}

/// Draw breakpoints list
fn draw_breakpoints_list(frame: &mut Frame, area: Rect, app: &mut App)
{
    let rows: Vec<Row> = app
        .cached_breakpoints
        .iter()
        .map(|bp| {
            let state_str = if bp.enabled {
                if bp.state == ferros_core::BreakpointState::Resolved {
                    "●"
                } else {
                    "○"
                }
            } else {
                "-"
            };

            let kind_str = match bp.kind {
                ferros_core::BreakpointKind::Software => "SW",
                ferros_core::BreakpointKind::Hardware => "HW",
                ferros_core::BreakpointKind::Watchpoint => "WP",
            };

            Row::new(vec![
                Cell::from(format!("{}", bp.id.raw())),
                Cell::from(state_str),
                Cell::from(kind_str),
                Cell::from(format!("{}", bp.address)),
                Cell::from(format!("{}", bp.hit_count)),
            ])
        })
        .collect();

    let constraints: Box<[Constraint]> = vec![
        Constraint::Length(5),
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(18),
        Constraint::Length(5),
    ]
    .into_boxed_slice();

    let table = Table::new(rows, constraints)
        .block(Block::default().borders(Borders::ALL).title("Breakpoints"))
        .header(Row::new(vec![
            Cell::from("ID").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("E").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("K").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Address").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Hits").style(Style::default().add_modifier(Modifier::BOLD)),
        ]))
        .row_highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(table, area, &mut app.breakpoints_state);
}

/// Draw the call stack and frame locals view
pub fn draw_stack_view(frame: &mut Frame, area: Rect, app: &mut App)
{
    // Split area into stack frames (left) and frame info (right)
    let constraints: Box<[Constraint]> = match app.layout_preset {
        crate::app::LayoutPreset::Compact => Box::new([Constraint::Percentage(100)]),
        crate::app::LayoutPreset::Standard => Box::new([Constraint::Percentage(50), Constraint::Percentage(50)]),
        crate::app::LayoutPreset::Widescreen => Box::new([Constraint::Percentage(40), Constraint::Percentage(60)]),
    };

    let chunks = Layout::horizontal(constraints).split(area);

    // Draw stack frames on the left
    draw_stack_frames(frame, chunks[0], app);

    // Draw frame details on the right (if space available)
    if chunks.len() > 1 {
        draw_frame_details(frame, chunks[1], app);
    }
}

/// Draw the stack frames list
fn draw_stack_frames(frame: &mut Frame, area: Rect, app: &mut App)
{
    let frames = app.cached_stack_trace.as_deref().unwrap_or(&[]);

    if frames.is_empty() {
        let error = Paragraph::new("No stack trace available. Process may be running.")
            .block(Block::default().borders(Borders::ALL).title("Call Stack"))
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(error, area);
        return;
    }

    let rows: Vec<Row> = frames
        .iter()
        .map(|frame| {
            let prefix = if frame.kind.is_inlined() { "↪ " } else { "  " };
            let symbol_name = frame
                .symbol
                .as_ref()
                .map_or("<unknown>", ferros_core::SymbolName::display_name);
            let location_str = frame.location.as_ref().map_or_else(
                || format!("{}", frame.pc),
                |loc| {
                    if let Some(line) = loc.line {
                        format!("{}:{}", loc.file.split('/').next_back().unwrap_or(&loc.file), line)
                    } else {
                        loc.file.split('/').next_back().unwrap_or(&loc.file).to_string()
                    }
                },
            );

            Row::new(vec![
                Cell::from(format!("{prefix}#{}", frame.index)),
                Cell::from(symbol_name),
                Cell::from(location_str),
            ])
        })
        .collect();

    let constraints: Box<[Constraint]> = Box::new([Constraint::Length(5), Constraint::Min(20), Constraint::Min(20)]);

    let table = Table::new(rows, constraints)
        .block(Block::default().borders(Borders::ALL).title("Call Stack"))
        .header(Row::new(vec![
            Cell::from("Frame").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Function").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Location").style(Style::default().add_modifier(Modifier::BOLD)),
        ]))
        .row_highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(table, area, &mut app.stack_frames_state);
}

/// Draw frame details (locals, registers, etc.)
fn draw_frame_details(frame: &mut Frame, area: Rect, app: &App)
{
    let selected_idx = app.stack_frames_state.selected().unwrap_or(0);
    let selected_frame = app.cached_stack_trace.as_ref().and_then(|frames| frames.get(selected_idx));

    if let Some(selected_frame) = selected_frame {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Frame #", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{}", selected_frame.index)),
            ]),
            Line::from(""),
        ];

        if let Some(ref symbol) = selected_frame.symbol {
            lines.push(Line::from(vec![
                Span::styled("Function: ", Style::default().fg(Color::Yellow)),
                Span::raw(symbol.display_name()),
            ]));
        }

        if let Some(ref location) = selected_frame.location {
            lines.push(Line::from(vec![
                Span::styled("File: ", Style::default().fg(Color::Yellow)),
                Span::raw(location.file.clone()),
            ]));
            if let Some(line) = location.line {
                lines.push(Line::from(vec![
                    Span::styled("Line: ", Style::default().fg(Color::Yellow)),
                    Span::raw(format!("{line}")),
                ]));
            }
        }

        // Display function parameters if available
        if !selected_frame.parameters.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Parameters: ", Style::default().fg(Color::Yellow)),
            ]));
            for param in &selected_frame.parameters {
                let param_str = match (&param.name, &param.type_name) {
                    (Some(name), Some(ty)) => format!("  {}: {}", name, ty),
                    (Some(name), None) => format!("  {}", name),
                    (None, Some(ty)) => format!("  <unnamed>: {}", ty),
                    (None, None) => "  <unknown>".to_string(),
                };
                lines.push(Line::from(Span::raw(param_str)));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("PC: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{}", selected_frame.pc)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("SP: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{}", selected_frame.sp)),
        ]));
        if selected_frame.fp.value() != 0 {
            lines.push(Line::from(vec![
                Span::styled("FP: ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{}", selected_frame.fp)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{:?}", selected_frame.status)),
        ]));

        let details = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Frame Details"))
            .style(Style::default().fg(Color::White));

        frame.render_widget(details, area);
    } else {
        let error_widget = Paragraph::new("No frame selected")
            .block(Block::default().borders(Borders::ALL).title("Frame Details"))
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(error_widget, area);
    }
}

/// Draw the timeline/log panel
pub fn draw_timeline(frame: &mut Frame, area: Rect, app: &App)
{
    let viewport_height = area.height.saturating_sub(2) as usize;
    let mut timeline_lines = Vec::new();

    if app.timeline_log.is_empty() {
        timeline_lines.push(Line::from("No timeline events yet."));
    } else {
        let start_idx = app.timeline_log.len().saturating_sub(viewport_height);
        for entry in app.timeline_log.iter().skip(start_idx) {
            let elapsed = entry.timestamp.elapsed();
            let time_str = format!("{:6.2}s", elapsed.as_secs_f64());

            let kind_color = match entry.kind {
                crate::app::TimelineEntryKind::Resume => Color::Green,
                crate::app::TimelineEntryKind::BreakpointHit => Color::Yellow,
                crate::app::TimelineEntryKind::Signal => Color::Magenta,
                crate::app::TimelineEntryKind::Output => Color::Cyan,
                crate::app::TimelineEntryKind::Stop | crate::app::TimelineEntryKind::Error => Color::Red,
            };

            let kind_str = match entry.kind {
                crate::app::TimelineEntryKind::Stop => "STOP",
                crate::app::TimelineEntryKind::Resume => "RESUME",
                crate::app::TimelineEntryKind::BreakpointHit => "BP",
                crate::app::TimelineEntryKind::Signal => "SIG",
                crate::app::TimelineEntryKind::Output => "OUT",
                crate::app::TimelineEntryKind::Error => "ERR",
            };

            timeline_lines.push(Line::from(vec![
                Span::styled(time_str, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(
                    format!("[{kind_str}]"),
                    Style::default().fg(kind_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(entry.message.clone()),
            ]));
        }
    }

    let timeline = Paragraph::new(timeline_lines)
        .block(Block::default().borders(Borders::ALL).title("Timeline"))
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: true });

    frame.render_widget(timeline, area);
}

/// Draw the help page
pub fn draw_help(frame: &mut Frame, area: Rect, _app: &App)
{
    let mut lines = Vec::new();

    // Title
    lines.push(Line::from(vec![
        Span::styled("Ferros Debugger - Help", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(""));

    // Navigation Section
    lines.push(Line::from(vec![
        Span::styled("VIEW NAVIGATION (Number Keys)", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  Press number keys to switch between views:"));
    lines.push(Line::from("  1 - Overview: Debugger status and process information"));
    lines.push(Line::from("  2 - Registers: CPU registers (PC, SP, FP, general registers)"));
    lines.push(Line::from("  3 - Threads: All threads in the process"));
    lines.push(Line::from("  4 - Memory Regions: Memory map of the process"));
    lines.push(Line::from("  5 - Output: Process stdout/stderr"));
    lines.push(Line::from("  6 - Source: Source code view with breakpoints"));
    lines.push(Line::from("  7 - Stack: Call stack and frame details"));
    lines.push(Line::from("  8 - Timeline: Event log of debugger operations"));
    lines.push(Line::from("  9 - Help: This help page"));
    lines.push(Line::from(""));

    // Navigation within views
    lines.push(Line::from(vec![
        Span::styled("WITHIN-VIEW NAVIGATION", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  ↑/↓ - Navigate up/down in current view (registers, threads, stack, etc.)"));
    lines.push(Line::from("  n - Next frame (in stack view)"));
    lines.push(Line::from("  p - Previous frame (in stack view)"));
    lines.push(Line::from(""));

    // Program Control
    lines.push(Line::from(vec![
        Span::styled("PROGRAM CONTROL", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  s - Suspend: Stop the process execution"));
    lines.push(Line::from("  r - Resume: Continue execution from current position"));
    lines.push(Line::from("  Note: Process must be stopped to inspect registers, stack, or source"));
    lines.push(Line::from(""));

    // Breakpoints
    lines.push(Line::from(vec![
        Span::styled("BREAKPOINTS", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  b - Toggle breakpoint:"));
    lines.push(Line::from("      • In Source view: at selected/current line"));
    lines.push(Line::from("      • In Stack view: at selected frame's PC"));
    lines.push(Line::from("      • Other views: at current PC"));
    lines.push(Line::from("  B - Open breakpoint editor to add breakpoints manually"));
    lines.push(Line::from(""));

    // Command Palette
    lines.push(Line::from(vec![
        Span::styled("COMMAND PALETTE (:)", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  Press ':' to open the command palette"));
    lines.push(Line::from("  Commands for breakpoint management:"));
    lines.push(Line::from("    break <address>  or  b <address>  - Add breakpoint at address (hex: 0x1000)"));
    lines.push(Line::from("    delete <id>      or  d <id>       - Remove breakpoint by ID"));
    lines.push(Line::from("    enable <id>      or  e <id>       - Enable a disabled breakpoint"));
    lines.push(Line::from("    disable <id>                        - Disable a breakpoint"));
    lines.push(Line::from("  Commands for navigation:"));
    lines.push(Line::from("    frame <index>    or  f <index>    - Jump to specific stack frame"));
    lines.push(Line::from("    thread <id>      or  t <id>       - Switch active thread"));
    lines.push(Line::from("  Other commands:"));
    lines.push(Line::from("    help             or  h            - Show this help"));
    lines.push(Line::from("  Use ↑/↓ in command palette to navigate command history"));
    lines.push(Line::from(""));

    // Other Shortcuts
    lines.push(Line::from(vec![
        Span::styled("OTHER SHORTCUTS", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  ? or h - Toggle help page"));
    lines.push(Line::from("  l - Cycle layout presets (Compact/Standard/Widescreen)"));
    lines.push(Line::from("  Esc - Quit debugger (or close command palette/breakpoint editor)"));
    lines.push(Line::from("  Ctrl+Q - Force quit"));
    lines.push(Line::from(""));

    // Tips
    lines.push(Line::from(vec![
        Span::styled("TIPS", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  • Use number keys (1-9) for quick view switching"));
    lines.push(Line::from("  • Suspend the process (s) before inspecting state"));
    lines.push(Line::from("  • In Stack view, select a frame to load its source code"));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Source View (6):", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from("  • Source code loads automatically when process is stopped"));
    lines.push(Line::from("  • Navigate to Stack view (7) and select a frame to change source"));
    lines.push(Line::from("  • Use ↑/↓ to scroll, 'b' to toggle breakpoint at selected line"));
    lines.push(Line::from("  • Current execution line is highlighted in yellow"));
    lines.push(Line::from("  • Breakpoints are shown with ● in the source view"));
    lines.push(Line::from(""));
    lines.push(Line::from("  • Timeline view shows chronological log of all events"));
    lines.push(Line::from("  • For best debugging, build programs with debug symbols"));

    let help_widget = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: true });

    frame.render_widget(help_widget, area);
}
