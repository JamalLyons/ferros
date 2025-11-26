//! UI rendering logic

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, ViewMode};

/// Draw the UI
pub fn draw(frame: &mut Frame, app: &mut App)
{
    // Use boxed slice to avoid large stack array warning
    // Make footer taller if there's an error message to display
    let footer_height = if app.error_message.is_some() {
        5 // Extra space for wrapped error messages
    } else {
        3
    };
    
    let constraints: Box<[Constraint]> = Box::new([
        Constraint::Length(3), // Header
        Constraint::Min(0),    // Main content
        Constraint::Length(footer_height), // Footer/status (taller if error)
    ]);
    let chunks = Layout::vertical(constraints).split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_main_content(frame, chunks[1], app);
    draw_footer(frame, chunks[2], app);
}

/// Draw the header bar
fn draw_header(frame: &mut Frame, area: Rect, app: &App)
{
    let title = if app.debugger.is_attached() {
        if let Some(pid) = app.pid {
            format!("Ferros Debugger - Attached (PID: {pid})")
        } else {
            "Ferros Debugger - Attached".to_string()
        }
    } else {
        "Ferros Debugger - Not Attached".to_string()
    };

    let header = Paragraph::new(title)
        .block(Block::default().borders(Borders::ALL).title("Ferros"))
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    frame.render_widget(header, area);
}

/// Draw the main content area
fn draw_main_content(frame: &mut Frame, area: Rect, app: &mut App)
{
    // Draw command palette overlay if active
    if app.command_palette_active {
        crate::widgets::draw_command_palette(frame, area, app);
        return;
    }

    // Draw breakpoint editor overlay if active
    if app.breakpoint_editor.is_some() {
        crate::widgets::draw_breakpoint_editor(frame, area, app);
        return;
    }

    // Draw main content based on view mode and layout
    match app.view_mode {
        ViewMode::Overview => crate::widgets::draw_overview(frame, area, app),
        ViewMode::Registers => crate::widgets::draw_registers(frame, area, app),
        ViewMode::Threads => crate::widgets::draw_threads(frame, area, app),
        ViewMode::MemoryRegions => crate::widgets::draw_memory_regions(frame, area, app),
        ViewMode::Output => crate::widgets::draw_output(frame, area, app),
        ViewMode::Source => crate::widgets::draw_source_view(frame, area, app),
        ViewMode::Stack => crate::widgets::draw_stack_view(frame, area, app),
        ViewMode::Timeline => crate::widgets::draw_timeline(frame, area, app),
        ViewMode::Help => crate::widgets::draw_help(frame, area, app),
    }
}

/// Draw the footer with help text
fn draw_footer(frame: &mut Frame, area: Rect, app: &App)
{
    let help_text = match app.view_mode {
        ViewMode::Overview => {
            "1:Overview 2:Regs 3:Threads 4:Memory 5:Output 6:Source 7:Stack 8:Timeline | :Cmd | s:Suspend r:Resume \
             b:Breakpoint B:EditBP l:Layout Esc:Quit"
        }
        ViewMode::Registers | ViewMode::Threads | ViewMode::MemoryRegions => {
            "↑/↓:Navigate | 1-8:Switch View | :Cmd | s:Suspend r:Resume b:Breakpoint | Esc:Quit"
        }
        ViewMode::Output => "↑/↓:Scroll | 1-8:Switch View | :Cmd | s:Suspend r:Resume | Esc:Quit",
        ViewMode::Source => "↑/↓:Scroll | 1-8:Switch View | :Cmd | b:ToggleBP | Esc:Quit",
        ViewMode::Stack => "↑/↓/n/p:Navigate | 1-8:Switch View | :Cmd | f:Frame | Esc:Quit",
        ViewMode::Timeline => "↑/↓:Scroll | 1-8:Switch View | :Cmd | Esc:Quit",
        ViewMode::Help => "Press ? or h to close help | 1-8:Switch View | Esc:Quit",
    };

    let mut footer_lines = vec![Line::from(help_text)];

    if let Some(ref error) = app.error_message {
        // Split long error messages into multiple lines to avoid truncation
        let max_width = area.width.saturating_sub(4); // Account for borders
        let error_text = format!("Error: {error}");
        
        // Break error into chunks that fit the width
        let mut error_lines = Vec::new();
        let mut current_line = String::new();
        
        for word in error_text.split_whitespace() {
            let test_line = if current_line.is_empty() {
                word.to_string()
            } else {
                format!("{} {}", current_line, word)
            };
            
            if test_line.len() as u16 <= max_width {
                current_line = test_line;
            } else {
                if !current_line.is_empty() {
                    error_lines.push(Line::from(vec![
                        Span::styled(current_line, Style::default().fg(Color::Red))
                    ]));
                }
                current_line = word.to_string();
            }
        }
        
        if !current_line.is_empty() {
            error_lines.push(Line::from(vec![
                Span::styled(current_line, Style::default().fg(Color::Red))
            ]));
        }
        
        footer_lines.extend(error_lines);
    }

    let footer = Paragraph::new(footer_lines)
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .style(Style::default().fg(Color::White))
        .wrap(ratatui::widgets::Wrap { trim: true });

    frame.render_widget(footer, area);
}
