//! Widget components for displaying debugger information

use ferros_core::types::{Architecture, StopReason};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::App;

/// Draw the overview screen
pub fn draw_overview(frame: &mut Frame, area: Rect, app: &App)
{
    let chunks = Layout::vertical([
        Constraint::Length(10), // Debugger info
        Constraint::Min(0),     // Status
    ])
    .split(area);

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
            Span::raw(if app.debugger.is_stopped() { "Yes" } else { "No" }),
        ]),
        Line::from(vec![
            Span::styled("Stop Reason: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{:?}", app.debugger.stop_reason())),
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
    let status_text = if app.debugger.is_attached() {
        match app.debugger.stop_reason() {
            StopReason::Running => "Process is running",
            StopReason::Suspended => "Process is suspended",
            StopReason::Signal(sig) => &format!("Stopped by signal: {}", sig),
            StopReason::Breakpoint(addr) => &format!("Hit breakpoint at 0x{:x}", addr),
            StopReason::Exited(code) => &format!("Process exited with code: {}", code),
            StopReason::Unknown => "Stopped for unknown reason",
        }
    } else {
        "Not attached to a process"
    };

    let status = Paragraph::new(status_text)
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
            let error = Paragraph::new(format!("Error reading registers: {}", e))
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
                rows.push(Row::new(vec![
                    Cell::from(format!("X{}", i)),
                    Cell::from(format!("0x{:016x}", val)),
                    Cell::from(""),
                ]));
            }
        }
        Architecture::X86_64 => {
            let reg_names = [
                "RAX", "RBX", "RCX", "RDX", "RSI", "RDI", "R8", "R9", "R10", "R11", "R12", "R13", "R14", "R15",
            ];
            for (i, val) in registers.general.iter().enumerate() {
                if i < reg_names.len() {
                    rows.push(Row::new(vec![
                        Cell::from(reg_names[i]),
                        Cell::from(format!("0x{:016x}", val)),
                        Cell::from(""),
                    ]));
                }
            }
        }
        Architecture::Unknown(_) => {
            for (i, val) in registers.general.iter().enumerate() {
                rows.push(Row::new(vec![
                    Cell::from(format!("R{}", i)),
                    Cell::from(format!("0x{:016x}", val)),
                    Cell::from(""),
                ]));
            }
        }
    }

    let table = Table::new(rows, [Constraint::Length(10), Constraint::Length(20), Constraint::Length(20)])
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
            let error = Paragraph::new(format!("Error reading threads: {}", e))
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
            let is_active = active_thread.map(|t| t == *thread).unwrap_or(false);
            let prefix = if is_active { "→ " } else { "  " };
            Row::new(vec![
                Cell::from(format!("{}{}", prefix, i)),
                Cell::from(format!("{}", thread.raw())),
                Cell::from(if is_active { "Active" } else { "" }),
            ])
        })
        .collect();

    let table = Table::new(rows, [Constraint::Length(10), Constraint::Length(20), Constraint::Length(10)])
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
            let error = Paragraph::new(format!("Error reading memory regions: {}", e))
                .block(Block::default().borders(Borders::ALL).title("Memory Regions"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return;
        }
    };

    let rows: Vec<Row> = regions
        .iter()
        .map(|region| {
            let size_kb = region.size() / 1024;
            Row::new(vec![
                Cell::from(format!("{}", region.id.value())),
                Cell::from(format!("{}", region.start)),
                Cell::from(format!("{}", region.end)),
                Cell::from(format!("{} KB", size_kb)),
                Cell::from(region.permissions.clone()),
                Cell::from(region.name.as_deref().unwrap_or("").to_string()),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Min(0),
        ],
    )
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
    // For now, show a message that output capture is not yet implemented
    // In the future, this would show captured stdout/stderr from the process
    let mut output_text = vec![
        Line::from(vec![
            Span::styled("Process Output", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];

    if let Some(pid) = app.pid {
        output_text.push(Line::from(format!("Process PID: {}", pid)));
        output_text.push(Line::from(""));
    }

    if app.process_output.is_empty() {
        output_text.extend(vec![
            Line::from("Process output capture is not yet implemented."),
            Line::from(""),
            Line::from("The process output should be visible in the terminal"),
            Line::from("where you launched ferros. However, when the TUI"),
            Line::from("enters alternate screen mode, output may be hidden."),
            Line::from(""),
            Line::from("To see process output:"),
            Line::from("  • Use 'ferros launch --headless' to see output directly"),
            Line::from("  • Or check the terminal after quitting the TUI"),
            Line::from(""),
            Line::from(vec![
                Span::styled("Note: ", Style::default().fg(Color::Yellow)),
                Span::raw("Full output capture in TUI is planned for a future release."),
            ]),
        ]);
    } else {
        output_text.push(Line::from("Captured output:"));
        output_text.push(Line::from(""));
        output_text.extend(
            app.process_output
                .iter()
                .map(|line| Line::from(line.as_str())),
        );
    }

    let output = Paragraph::new(output_text)
        .block(Block::default().borders(Borders::ALL).title("Process Output"))
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: true });

    frame.render_widget(output, area);
}
