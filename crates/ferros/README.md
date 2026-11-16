# ferros

The command-line interface for the Ferros debugger.

## Overview

`ferros` is the user-facing CLI binary for the Ferros debugging platform. It parses commands, initializes structured logging via `ferros-utils`, instantiates the platform backend (on macOS, `ferros-macos`), and controls target execution through `ferros-core::Session`.

## Building

Ferros is not published to crates.io yet. Build from source:

```bash
cargo build -p ferros
```

On macOS, running or debugging external processes requires the binary to be ad-hoc signed with the debugger entitlement (`crates/ferros-macos/ferros.entitlements`):

```bash
just sign-debugger target/debug/ferros
# or: codesign --entitlements crates/ferros-macos/ferros.entitlements --force --sign - target/debug/ferros
```

## Usage

```bash
# Launch a program, stopped at its entry, and open a session on it.
# Program arguments go after `--`.
ferros launch <program> [-- args...]

# Attach to a running process by PID.
ferros attach <pid>

# Run commands non-interactively (batch mode), then end the session.
ferros launch --command continue <program>
ferros launch --batch script.ferros --command registers <program> -- arg1 arg2

# Feed a file to the program's stdin.
ferros launch --stdin input.txt <program>

# Print the directory that holds the log files.
ferros find-logs
```

`launch` and `attach` take the same options:

- **`--batch <FILE>`**: run the commands in the file, one per line.
- **`--command <CMD>`** (`-c`, repeatable): run this command, after the `--batch` file.

`launch` also takes **`--stdin <FILE>`**: the program reads FILE as its stdin (see
[The program's stdin](#the-programs-stdin)). The file must exist; `run` reopens it from the start
for each relaunch.

The global options `--log-level <LEVEL>` and `--log-format <pretty|json>` configure the log file
(`~/.ferros/YYYY-MM-DD-ferros.log`). Logs never go to the terminal, so the session output is plain
text without colour or timestamps.

## Input modes

- **Interactive**: stdin is a terminal and neither `--batch` nor `--command` is given. Line
  editing and history come from rustyline. The history file is `$FERROS_HISTORY` when that
  variable is set and not empty, otherwise `~/.ferros_history`.
- **Piped**: stdin is not a terminal. `ferros` prints the prompt `(ferros) ` before reading each
  line and echoes the line, so a driving program can wait for the prompt before sending the next
  command.
- **Batch**: the `--batch` lines, then each `--command`, are run in order; stdin is not read. Each
  command is printed as `(ferros) <command>` before its output.

In every mode, empty lines and lines starting with `#` are skipped. A failing command prints
`error: <message>` on stderr and the next command still runs. `ferros` exits with status 1 if any
command failed, 0 otherwise.

At `quit` or the end of input, a launched program is killed (`killed process <pid>`) and an attached
one is detached (`detached from process <pid>`) and keeps running.

The debuggee's stdout and stderr are those of `ferros`: its output appears in the terminal (or the
same pipe) between the lines `ferros` prints.

### The program's stdin

A launched program's stdin depends on where `ferros` reads its commands:

| Mode | Program's stdin |
|---|---|
| Interactive (terminal) | The terminal, in a background process group (see [Ctrl-C](#ctrl-c)) |
| Piped | `/dev/null`: the commands on the pipe are for `ferros` only, so the program reads end of file |
| Batch (`--batch`, `--command`) | `ferros`'s stdin, which `ferros` does not read in this mode |
| Any mode with `--stdin <FILE>` | FILE |

To give a program input in interactive or piped mode, use `--stdin`. An attached process
keeps whatever stdin it already has.

## Commands

| Command | Aliases | What it does |
|---|---|---|
| `run` | `r` | Resume from the entry stop and wait for the next stop; after the program exited, relaunch it stopped at entry (breakpoints are cleared with the old process) |
| `continue` | `c` | Resume and wait for the next stop. A signal the program stopped for is passed on, except `SIGINT` |
| `interrupt` | | Stop the program (Ctrl-C does this while it runs) |
| `kill` | | Terminate the program with `SIGKILL` |
| `status` | | `status: stopped (pid <pid>, thread <tid>)` and the last stop, `status: exited` and how, or `status: no process` |
| `quit` | `q`, `exit` | End the session |
| `stepi` | `si` | Execute one instruction of the selected thread |
| `break *<address>` | `b` | Add a software breakpoint at a runtime address (`0x` hex or decimal) |
| `delete <id>` | `d` | Remove a breakpoint |
| `breakpoints` | | List breakpoints with hit counts |
| `registers` | `regs` | `pc`, `sp`, `fp`, `lr`, `cpsr` and `x0`..`x28` of the selected thread |
| `memory <address> [len]` | `x` | Hexdump `len` bytes (default 64), 16 per row |
| `threads` | | List the threads; `*` marks the selected one |
| `thread <index>` | | Select a thread by its index in `threads` |
| `images` | | Load address and path of each loaded image (the address includes the ASLR slide) |
| `help [command]` | `h`, `?` | List the commands, or show one command's usage |

Stops print as one stable line each:

```text
stopped: breakpoint 1 at 0x100000f2c (thread 8963)
stopped: step at 0x100000f30 (thread 8963)
stopped: signal 11 (thread 8963)
stopped: interrupted
process exited with status 0
process killed by signal 9
```

### Ctrl-C

Ctrl-C never ends `ferros`. While the program runs (`continue`, `run`, `stepi`) it interrupts the
program and returns to the prompt; at an interactive prompt it discards the line. Launched programs
have a separate background process group. Ferros keeps the terminal foreground, so terminal Ctrl-C
reaches only ferros. Its handler calls `Session::interrupt`, reported as `stopped: interrupted`.
A real `SIGINT` sent directly to the program (for example, `kill -INT <pid>`) remains
`stopped: signal 2`. `continue` suppresses that held `SIGINT` rather than delivering it.

The trade-off is terminal input: a background program reading inherited terminal stdin stops with
`SIGTTIN`, reported as `stopped: signal 21`. Use `ferros launch --stdin FILE <program>` to supply
input. Batch-mode stdin from a pipe or file still works; batch-mode terminal reads have the same
background-group restriction. Attaching leaves the target's process group unchanged.

### Adding a command

Commands live in `src/repl/commands/`, one module per area (`process.rs`, `inspect.rs`,
`threads.rs`, `breakpoints.rs`, `step.rs`, `images.rs`, `help.rs`). A command is a unit struct
implementing `ReplCommand` (`names`, `summary`, `help`, `run`). To add one, put it in a new file and
add one line to the `registry!` list in `src/repl/commands/mod.rs`.

## Platform Support

- **macOS (Apple Silicon arm64)**: Currently supported backend (`crates/ferros-macos`).
- **Linux & Windows**: Planned.

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.
