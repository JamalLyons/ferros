//! Terminal User Interface initialization and management

use std::fs::File;
use std::io::{self, BufRead, BufReader, Stdout};
use std::panic;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ferros_core::Debugger;
use ferros_utils::{info, warn};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{App, ProcessOutputSource};
use crate::event::Event;

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
    ///
    /// # Errors
    ///
    /// Returns an error if terminal initialization fails (raw mode, alternate screen, etc.)
    ///
    /// # Panics
    ///
    /// May panic if terminal restoration fails during panic hook setup
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
    ///
    /// # Errors
    ///
    /// Returns an error if terminal drawing fails or terminal restoration fails
    pub async fn run(&mut self, debugger: Box<dyn Debugger>, pid: Option<u32>, was_launched: bool) -> io::Result<()>
    {
        use std::io::Write;

        if let Some(pid) = pid {
            info!("Ferros TUI started (PID: {}, launched: {})", pid, was_launched);
        } else {
            info!("Ferros TUI started");
        }

        let mut app = App::new(debugger, pid, was_launched);
        let mut event_handler = crate::event::EventHandler::new();
        let mut background_tasks = spawn_background_tasks(&mut app, event_handler.sender());

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
                    Event::Key(key_event) => {
                        if app.handle_key_event(key_event) {
                            break;
                        }
                    }
                    Event::Tick => {
                        app.tick();
                    }
                    Event::ProcessOutput { source, line } => {
                        app.push_process_output(source, &line.clone());
                    }
                    Event::Debugger(debugger_event) => {
                        app.handle_debugger_event(&debugger_event);
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

        // Restore terminal IMMEDIATELY so user sees normal output right away
        // This prevents the frozen appearance during cleanup
        Self::restore()?;

        // Stop the event handler to allow the program to exit
        event_handler.stop();

        // Don't wait indefinitely for output tasks - use timeout
        // This prevents blocking if tasks are stuck reading from pipes
        let output_handles = std::mem::take(&mut background_tasks);
        let timeout_result = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            for handle in output_handles {
                if let Err(e) = handle.await {
                    warn!("Process output task exited with error: {e}");
                }
            }
        })
        .await;
        if timeout_result.is_err() {
            warn!("Output tasks didn't finish in time, dropping");
        }

        // Cleanup after terminal is restored (async, non-blocking)
        // User can see what's happening in normal terminal mode
        app.cleanup().await;

        // Flush stdout to ensure any messages are visible
        let _ = std::io::stdout().flush();

        info!("Ferros TUI closed");

        // Print a message so user knows what happened
        // Note: This prints after restoring terminal, so it will be visible
        if app.was_launched {
            if let Some(pid) = app.pid {
                println!("\nDebugger detached. Process {pid} was terminated.");
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
    ///
    /// # Errors
    ///
    /// Returns an error if terminal restoration fails (disabling raw mode, leaving alternate screen, etc.)
    pub fn restore() -> io::Result<()>
    {
        disable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
        Ok(())
    }
}

fn spawn_background_tasks(app: &mut App, sender: mpsc::Sender<Event>) -> Vec<JoinHandle<()>>
{
    let mut handles = Vec::new();

    if let Some(stdout) = app.debugger.take_process_stdout() {
        handles.push(spawn_output_reader(stdout, ProcessOutputSource::Stdout, sender.clone()));
    }

    if let Some(stderr) = app.debugger.take_process_stderr() {
        handles.push(spawn_output_reader(stderr, ProcessOutputSource::Stderr, sender.clone()));
    }

    if let Some(events) = app.debugger.take_event_receiver() {
        handles.push(spawn_debugger_event_forwarder(events, sender));
    }

    handles
}

fn spawn_output_reader(file: File, source: ProcessOutputSource, sender: mpsc::Sender<Event>) -> JoinHandle<()>
{
    tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(file);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if sender.blocking_send(Event::ProcessOutput { source, line }).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    warn!("Failed to read process output: {err}");
                    break;
                }
            }
        }
    })
}

fn spawn_debugger_event_forwarder(
    receiver: ferros_core::events::DebuggerEventReceiver,
    sender: mpsc::Sender<Event>,
) -> JoinHandle<()>
{
    tokio::task::spawn_blocking(move || {
        while let Ok(event) = receiver.recv() {
            if sender.blocking_send(Event::Debugger(event)).is_err() {
                break;
            }
        }
    })
}

impl Drop for Tui
{
    fn drop(&mut self)
    {
        let _ = Self::restore();
    }
}
