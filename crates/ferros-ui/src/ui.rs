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
    let constraints: Box<[Constraint]> = Box::new([
        Constraint::Length(3), // Header
        Constraint::Min(0),    // Main content
        Constraint::Length(3), // Footer/status
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
    }
}

/// Draw the footer with help text
fn draw_footer(frame: &mut Frame, area: Rect, app: &App)
{
    let help_text = match app.view_mode {
        ViewMode::Overview => {
            "1:Overview 2:Regs 3:Threads 4:Memory 5:Output 6:Source 7:Stack 8:Timeline | :Cmd | s:Suspend r:Resume \
             b:Breakpoint B:EditBP l:Layout Ctrl+Q:Quit"
        }
        ViewMode::Registers | ViewMode::Threads | ViewMode::MemoryRegions => {
            "↑/↓:Navigate | 1-8:Switch View | :Cmd | s:Suspend r:Resume b:Breakpoint | Ctrl+Q:Quit"
        }
        ViewMode::Output => "↑/↓:Scroll | 1-8:Switch View | :Cmd | s:Suspend r:Resume | Ctrl+Q:Quit",
        ViewMode::Source => "↑/↓:Scroll | 1-8:Switch View | :Cmd | b:ToggleBP | Ctrl+Q:Quit",
        ViewMode::Stack => "↑/↓/n/p:Navigate | 1-8:Switch View | :Cmd | f:Frame | Ctrl+Q:Quit",
        ViewMode::Timeline => "↑/↓:Scroll | 1-8:Switch View | :Cmd | Ctrl+Q:Quit",
    };

    let mut footer_content = vec![Span::raw(help_text)];

    if let Some(ref error) = app.error_message {
        footer_content.push(Span::raw(" | "));
        footer_content.push(Span::styled(format!("Error: {error}"), Style::default().fg(Color::Red)));
    }

    let footer = Paragraph::new(Line::from(footer_content))
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .style(Style::default().fg(Color::White));

    frame.render_widget(footer, area);
}
