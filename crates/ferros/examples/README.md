# Test Target Programs

This directory contains test programs designed to be debugged with Ferros.

## Available Test Programs

1. **`test_target`**: Full-featured test program with threads, signal handling, and complex behavior
2. **`simple_test`**: Minimal test program with no external dependencies

## Building the Test Programs

```bash
# Build the full-featured test program
cargo build --example test_target

# Build the simple test program
cargo build --example simple_test
```

The binaries will be located at:
```
target/debug/examples/test_target
target/debug/examples/simple_test
```

## Running the Test Programs

### Option 1: Run it standalone (then attach with Ferros)

```bash
# Terminal 1: Run the test program
cargo run --example test_target
# OR
cargo run --example simple_test

# Terminal 2: Attach to it with Ferros TUI
ferros attach $(pgrep -f test_target)
# OR
ferros attach $(pgrep -f simple_test)
```

### Option 2: Launch it directly with Ferros

```bash
# Launch with TUI (default)
cargo build --example test_target
ferros launch target/debug/examples/test_target

# OR launch simple_test
cargo build --example simple_test
ferros launch target/debug/examples/simple_test

# Launch in headless mode
ferros launch target/debug/examples/test_target --headless
```

## What the Test Programs Do

### `test_target` (Full-featured)

Provides various debugging scenarios:

1. **Long-running loop**: Runs indefinitely until interrupted (Ctrl+C)
2. **Function calls**: Multiple nested function calls (`process_value`, `validate_result`)
3. **Variable manipulation**: Counter and accumulator variables that change over time
4. **Memory allocation**: Periodically allocates memory (vectors, strings)
5. **Thread spawning**: Spawns a worker thread that does computation
6. **Signal handling**: Handles Ctrl+C gracefully

### `simple_test` (Minimal)

A simpler version with no external dependencies:

1. **Long-running loop**: Runs indefinitely
2. **Function calls**: Nested function calls (`add_to_sum`, `clamp_result`)
3. **Variable manipulation**: Counter and sum variables
4. **Memory allocation**: Periodically allocates memory

## Testing the TUI

Once attached or launched with Ferros:

- **Press `1`**: View Overview (debugger info, status)
- **Press `2`**: View Registers (CPU registers)
- **Press `3`**: View Threads (all threads in the process)
- **Press `4`**: View Memory Regions (memory map)
- **Press `s`**: Suspend the process
- **Press `r`**: Resume the process
- **Press `q` or `Esc`**: Quit the TUI

## Example Debugging Session

```bash
# Terminal 1: Build and run test program
cargo build --example test_target
./target/debug/examples/test_target

# Terminal 2: Attach with Ferros TUI
ferros attach $(pgrep -f test_target)

# In the TUI:
# - Switch to Registers view (press 2)
# - Suspend the process (press s)
# - Inspect registers
# - Resume the process (press r)
# - Switch to Threads view (press 3)
# - See both main thread and worker thread
```

## Notes

- The test program prints its PID when it starts, making it easy to attach
- It runs indefinitely, so you have plenty of time to attach and debug
- The worker thread provides a multi-threaded debugging scenario
- Memory allocations happen periodically, useful for testing memory inspection

