# ferros-utils

Shared utilities, logging, config, and helpers for Ferros.

## Overview

`ferros-utils` provides common utilities and helper functions used across the Ferros workspace, including:

- **Production-ready logging** using `tracing` with structured logging support
- Shared macros and traits
- Configuration management
- Common helper functions
- Cross-crate utilities

## Usage

Add `ferros-utils` to your `Cargo.toml`:

```toml
[dependencies]
ferros-utils = { path = "../ferros-utils" }
```

## Logging

### Quick Start

```rust
use ferros_utils::init_logging;

fn main() {
    // Initialize logging (reads from RUST_LOG env var)
    init_logging().expect("Failed to initialize logging");
    
    // Use tracing macros
    tracing::info!("Application started");
    tracing::debug!("Debug information");
    tracing::error!("An error occurred");
}
```

### Environment Variables

- `RUST_LOG`: Set log level filter (e.g., `debug`, `ferros_core=debug,ferros=info`)
- `FERROS_LOG_FORMAT`: Set output format (`json` or `pretty`, default: `pretty`)
- `FERROS_LOG_FILE`: Optional path to log file (if not set, logs only to console)

### Examples

```rust
use ferros_utils::{init_logging_with_level, LogFormat, LogLevel};

// Initialize with specific log level and format
init_logging_with_level(LogLevel::Debug, LogFormat::Pretty)
    .expect("Failed to initialize logging");

// Initialize with JSON output for production
init_logging_with_level(LogLevel::Info, LogFormat::Json)
    .expect("Failed to initialize logging");
```

### Log Formats

- **Pretty** (default): Human-readable format with colors, ideal for development
- **JSON**: Structured JSON output, ideal for production and log aggregation tools

### Features

- Structured logging with spans and fields
- Environment variable configuration
- Multiple output formats (pretty/JSON)
- File logging support (with daily rotation)
- Thread-safe and async-compatible
- Zero-cost when disabled

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.

