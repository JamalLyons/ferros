# platform

This module contains the platform specific implementations of the debugger.

Each platform has its own submodule that implements the `FerrosDebugger` trait
using that platform's native debugging APIs.

## Platforms

- **macOS**: Uses Mach APIs (`task_for_pid`, `thread_get_state`, etc.)
- **Linux**: TBA
- **Windows**: TBA