//! Event handling for the TUI

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind};
use ferros_core::events::DebuggerEvent;
use tokio::sync::mpsc;

use crate::app::ProcessOutputSource;

/// Events that can occur in the TUI
#[derive(Debug, Clone)]
pub enum Event
{
    /// Keyboard input event
    Key(KeyEvent),
    /// Tick event (for periodic updates)
    Tick,
    /// Captured process output ready to display
    ProcessOutput
    {
        source: ProcessOutputSource, line: String
    },
    /// Asynchronous debugger state change.
    Debugger(DebuggerEvent),
}

/// Event handler that reads from crossterm and produces TUI events
pub struct EventHandler
{
    receiver: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    should_stop: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl EventHandler
{
    /// Create a new event handler
    ///
    /// This spawns a background task that reads crossterm events
    /// and sends them to the async receiver.
    #[must_use]
    pub fn new() -> Self
    {
        let tick_rate = Duration::from_millis(250);
        let (sender, receiver) = mpsc::channel(100);
        let should_stop = Arc::new(AtomicBool::new(false));

        let sender_clone = sender.clone();
        let should_stop_clone = should_stop.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut last_tick = std::time::Instant::now();
            loop {
                // Check if we should stop
                if should_stop_clone.load(Ordering::Relaxed) {
                    break;
                }

                let timeout = tick_rate
                    .checked_sub(last_tick.elapsed())
                    .unwrap_or_else(|| Duration::from_secs(0));

                if event::poll(timeout).unwrap_or(false) {
                    if let Ok(CrosstermEvent::Key(key)) = event::read() {
                        if key.kind == KeyEventKind::Press {
                            // Use blocking send since we're in a blocking context
                            // If send fails (receiver dropped), break
                            if sender_clone.blocking_send(Event::Key(key)).is_err() {
                                break;
                            }
                        }
                    }
                }

                if last_tick.elapsed() >= tick_rate {
                    // If send fails (receiver dropped), break
                    if sender_clone.blocking_send(Event::Tick).is_err() {
                        break;
                    }
                    last_tick = std::time::Instant::now();
                }
            }
        });

        Self {
            receiver,
            sender,
            should_stop,
            handle,
        }
    }

    /// Stop the event handler gracefully
    ///
    /// This sets the stop flag and drops the receiver, allowing the background
    /// task to exit cleanly on its next iteration.
    pub fn stop(&mut self)
    {
        self.should_stop.store(true, Ordering::Relaxed);
        // Drop the receiver to signal the background task
        // We can't use std::mem::take because Receiver doesn't implement Default
        // Instead, we just drop it explicitly by replacing with a dummy receiver
        // The background task will detect the channel is closed when it tries to send
        drop(std::mem::replace(&mut self.receiver, {
            let (_sender, receiver) = mpsc::channel(1);
            receiver
        }));
    }

    /// Abort the event handler task immediately
    ///
    /// This forcefully terminates the background task without waiting for
    /// it to finish its current operation. Use this when you need immediate
    /// shutdown, such as during error handling or cleanup.
    ///
    /// ## Note
    ///
    /// After calling `abort()`, the event handler should not be used further.
    /// The receiver will be closed and no more events will be produced.
    pub fn abort(&mut self)
    {
        self.should_stop.store(true, Ordering::Relaxed);
        self.handle.abort();
        // Drop the receiver to ensure channel is closed
        drop(std::mem::replace(&mut self.receiver, {
            let (_sender, receiver) = mpsc::channel(1);
            receiver
        }));
    }

    /// Check if the event handler task is still running
    ///
    /// Returns `true` if the background task is still active, `false` if it has finished.
    /// This can be useful for monitoring the health of the event handler.
    #[must_use]
    pub fn is_running(&self) -> bool
    {
        !self.handle.is_finished()
    }

    /// Get the next event (async)
    pub async fn next(&mut self) -> Option<Event>
    {
        self.receiver.recv().await
    }

    /// Get a sender that can be used to push events into the queue.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<Event>
    {
        self.sender.clone()
    }
}

impl Default for EventHandler
{
    fn default() -> Self
    {
        Self::new()
    }
}

impl Drop for EventHandler
{
    fn drop(&mut self)
    {
        self.stop();
        // Note: We can't await the handle in Drop, but the task will exit
        // when should_stop is set or when the sender detects the receiver is dropped
    }
}
