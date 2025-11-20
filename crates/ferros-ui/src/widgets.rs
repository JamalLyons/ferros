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
