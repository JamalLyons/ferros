//! Example demonstrating how to use ferros-utils logging
//!
//! This example shows different ways to initialize and use logging:
//!
//! 1. Using environment variables (recommended)
//! 2. Using explicit initialization
//! 3. Using structured logging with fields
//! 4. Using spans for context

use ferros_utils::init_logging;

fn main()
{
    // Method 1: Initialize with environment variables (recommended)
    // Set RUST_LOG=debug to see debug messages
    // Set FERROS_LOG_FORMAT=json for JSON output
    init_logging().expect("Failed to initialize logging");

    tracing::info!("Application started");

    // Method 2: Initialize with explicit settings
    // Uncomment to override environment variables:
    // init_logging_with_level(LogLevel::Debug, LogFormat::Pretty)
    //     .expect("Failed to initialize logging");

    // Basic logging
    tracing::error!("This is an error message");
    tracing::warn!("This is a warning message");
    tracing::info!("This is an info message");
    tracing::debug!("This is a debug message (set RUST_LOG=debug to see)");
    tracing::trace!("This is a trace message (set RUST_LOG=trace to see)");

    // Structured logging with fields
    tracing::info!(user_id = 12345, action = "login", "User logged in");

    // Using spans for context
    let span = tracing::span!(tracing::Level::INFO, "process_attachment", pid = 12345);
    let _guard = span.enter();
    tracing::info!("Attaching to process");
    tracing::debug!("Reading registers");
    tracing::info!("Detaching from process");
    drop(_guard);

    // Logging with error context
    let result: Result<(), String> = Err("Something went wrong".to_string());
    if let Err(e) = result {
        tracing::error!(error = %e, "Operation failed");
    }

    tracing::info!("Application finished");
}
