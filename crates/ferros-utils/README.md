# ferros-utils

Logging for the Ferros workspace (`tracing`).

## Overview

`ferros-utils` is the workspace logging crate. It initializes `tracing` and re-exports the `tracing` macros. It does not define shared macros of its own, a config type, or other helpers.

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

### File logging for a session

The `ferros` CLI logs to a file only, never to the terminal:

```rust
use ferros_utils::{LogFormat, start_file_logging};

fn main() {
    // Hold the session until `main` returns: it owns the background worker that
    // writes the file, and dropping it flushes the records written just before exit.
    let _logging = start_file_logging(None, LogFormat::Pretty).ok();
    tracing::info!("Application started");
}
```

- The file is `YYYY-MM-DD-ferros.log` in `log_directory()`.
- `log_directory()` is `~/.ferros` when `HOME` is set and not empty, otherwise the OS temporary directory. It is always absolute: an empty `TMPDIR` means `/tmp`, and a relative one is joined onto the working directory. `ferros find-logs` prints this path's raw bytes.
- The flush on drop is bounded (about 100 ms to queue the shutdown, then up to about one second to drain). A worker still behind after that, for example behind a blocked file, is abandoned with its remaining records, so exit never hangs.
- `init_logging_to_file` and `init_logging_for_tui` write the same kind of file synchronously, for callers that cannot hold a `LoggingSession`; nothing needs flushing at exit.
- Each initializer installs the tracing subscriber, then forwards `log` records to it. If another `log` logger is already installed, that logger keeps the `log` records and tracing still works.

### Environment Variables

- `RUST_LOG`: Set log level filter (e.g., `debug`, `ferros_core=debug,ferros=info`)
- `FERROS_LOG_FORMAT`: Set output format (`json` or `pretty`, default: `pretty`)
- `FERROS_LOG_FILE`: Optional path to log file for `init_logging` (if not set, logs only to console)
- `FERROS_TEST_LOG_WRITE_DELAY_MS`: Test-only. Slows each write of the `start_file_logging` worker, so the exit tests can prove the last record is still queued when `main` returns.

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
- File logging to a path chosen at initialization (`FERROS_LOG_FILE`, or a dated file in `log_directory()` from `start_file_logging`, `init_logging_to_file`, or `init_logging_for_tui`)

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.

