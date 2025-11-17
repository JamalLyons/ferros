//! Terminal User Interface initialization and management

use std::io::{self, Stdout};
use std::panic;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ferros_core::Debugger;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::App;

/// Terminal User Interface for Ferros debugger
///
/// This struct manages the terminal state and provides methods to run
/// the interactive debugger interface.
pub struct Tui
{
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui
{
    /// Create a new TUI instance
    ///
    /// This initializes the terminal in raw mode and alternate screen,
    /// and sets up panic handling to restore the terminal on panic.
    pub fn new() -> io::Result<Self>
    {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        // Set up panic hook to restore terminal on panic
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            Self::restore().unwrap();
            original_hook(panic_info);
        }));

        Ok(Self { terminal })
    }

    /// Run the TUI event loop
    ///
    /// This starts the interactive debugger interface and handles user input
    /// until the user quits.
    pub async fn run(&mut self, debugger: Box<dyn Debugger>, pid: Option<u32>, was_launched: bool) -> io::Result<()>
    {
        use ferros_utils::info;

        if let Some(pid) = pid {
            info!("Ferros TUI started (PID: {}, launched: {})", pid, was_launched);
        } else {
            info!("Ferros TUI started");
        }

        let mut app = App::new(debugger, pid, was_launched);
        let mut event_handler = crate::event::EventHandler::new();

        loop {
            // Check if we should quit before drawing
            if app.should_quit {
                break;
            }

            self.terminal.draw(|frame| crate::ui::draw(frame, &mut app))?;

            // Check again after drawing
            if app.should_quit {
                break;
            }

            // Use a timeout to allow periodic checks
            match tokio::time::timeout(std::time::Duration::from_millis(100), event_handler.next()).await {
                Ok(Some(event)) => match event {
                    crate::event::Event::Key(key_event) => {
                        if app.handle_key_event(key_event) {
                            break;
                        }
                    }
                    crate::event::Event::Tick => {
                        app.tick();
                    }
                },
                Ok(None) => {
                    // Channel closed
                    break;
                }
                Err(_) => {
                    // Timeout - check should_quit and continue
                    if app.should_quit {
                        break;
                    }
                }
            }
        }

        info!("Ferros TUI closing");

        // Stop the event handler to allow the program to exit
        event_handler.stop();

        // Cleanup before restoring terminal
        app.cleanup();

        // Flush stdout to ensure any messages are visible
        use std::io::Write;
        let _ = std::io::stdout().flush();

        // Restore terminal to normal mode
        Self::restore()?;

        info!("Ferros TUI closed");

        // Print a message so user knows what happened
        // Note: This prints after restoring terminal, so it will be visible
        if app.was_launched {
            if let Some(pid) = app.pid {
                println!("\nDebugger detached. Process {} was terminated.", pid);
            }
        } else {
            println!("\nDebugger detached from process.");
        }

        Ok(())
    }

    /// Restore the terminal to its original state
    ///
    /// This should be called when exiting the TUI to ensure the terminal
    /// is left in a usable state.
    pub fn restore() -> io::Result<()>
    {
        disable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
        Ok(())
    }
}

impl Drop for Tui
{
    fn drop(&mut self)
    {
        let _ = Self::restore();
    }
}
