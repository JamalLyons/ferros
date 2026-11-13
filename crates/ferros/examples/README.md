# Test Target Programs

This directory contains standalone example programs designed to be debugged with Ferros.

## Available Programs

1. **`test_target`**: Full-featured test program with threads, signal handling, and complex loops.
2. **`simple_test`**: Minimal test program with loops and simple function calls.

## Building the Programs

```bash
# Build the full-featured test program
cargo build -p ferros --example test_target

# Build the simple test program
cargo build -p ferros --example simple_test
```

The compiled binaries will be located at:
```
target/debug/examples/test_target
target/debug/examples/simple_test
```

## Running with Ferros

> **Note**: The interactive terminal UI (`ferros-ui`) is currently parked and being rebuilt. Launching or attaching with the `ferros` CLI currently runs in headless mode: it inspects the target process at entry, prints its state, thread count, and memory regions, and then detaches.

### Option 1: Launch directly under Ferros

```bash
cargo run -p ferros -- launch target/debug/examples/test_target
```

### Option 2: Run standalone, then attach by PID

```bash
# Terminal 1: Run the test program
cargo run -p ferros --example test_target

# Terminal 2: Attach with Ferros
cargo run -p ferros -- attach $(pgrep -f test_target)
```

## Program Descriptions

### `test_target` (Full-featured)
- **Long-running loop**: Runs continuously until interrupted (Ctrl+C).
- **Function calls**: Nested function calls (`process_value`, `validate_result`).
- **Variable manipulation**: Counter and accumulator variables that update over time.
- **Memory allocation**: Periodically allocates heap memory (vectors, strings).
- **Thread spawning**: Spawns a background worker thread.
- **Signal handling**: Handles SIGINT gracefully.

### `simple_test` (Minimal)
- **Loop execution**: Runs a periodic loop with small sleeps.
- **Function calls**: Calls helper functions (`add_to_sum`, `clamp_result`).
- **Variable state**: Counter and accumulator variables.
- **Memory allocation**: Simple heap allocations.

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.
