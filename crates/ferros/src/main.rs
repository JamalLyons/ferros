use ferros_utils::init_logging;

fn main()
{
    // Initialize logging (reads from RUST_LOG env var)
    // Defaults to INFO level and Pretty format if not set
    if let Err(e) = init_logging() {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    // Now you can use tracing macros throughout your code
    tracing::info!("Hello, Ferros! 🦀");
    tracing::debug!("Debug information (only shown if RUST_LOG=debug)");
}
