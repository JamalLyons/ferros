//! Event handling for the TUI

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind};
use tokio::sync::mpsc;

/// Events that can occur in the TUI
#[derive(Debug, Clone, Copy)]
pub enum Event
{
    /// Keyboard input event
    Key(KeyEvent),
    /// Tick event (for periodic updates)
    Tick,
}

/// Event handler that reads from crossterm and produces TUI events
pub struct EventHandler
{
    receiver: mpsc::Receiver<Event>,
    should_stop: Arc<AtomicBool>,
    #[allow(dead_code)] // Kept for potential future use (e.g., aborting the task)
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
            should_stop,
            handle,
        }
    }

    /// Stop the event handler and wait for the background task to finish
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

    /// Get the next event (async)
    pub async fn next(&mut self) -> Option<Event>
    {
        self.receiver.recv().await
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
