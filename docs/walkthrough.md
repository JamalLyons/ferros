# A complete Phase 1 debugging session

Run this from the Ferros repository root on macOS arm64, with the repository's
Rust toolchain and Xcode command-line tools available. Cargo's configured runner
ad-hoc signs Ferros with its debugger entitlement; no sudo is needed. Developer
Tools access must already be enabled (see the repository's contributor guide).

## Build a small example project

Create a standalone project in a fresh temporary directory. Packed debug information
keeps its dSYM next to the executable so Ferros can find source lines and unwind data.
Keep this shell open: the following commands use `FERROS_DEMO`.

```sh
FERROS_DEMO=$(mktemp -d -t ferros-walkthrough)
mkdir -p "$FERROS_DEMO/src"
cat > "$FERROS_DEMO/Cargo.toml" <<'TOML'
[package]
name = "ferros_walkthrough"
version = "0.1.0"
edition = "2024"

[profile.dev]
debug = 2
split-debuginfo = "packed"
TOML
cat > "$FERROS_DEMO/src/main.rs" <<'RUST'
#[inline(never)]
fn add_one(value: u64) -> u64 {
    let result = std::hint::black_box(value + 1);
    result
}

fn main() {
    let seed = std::hint::black_box(40);
    let result = add_one(seed);
    println!("result: {result}");
}
RUST
cargo build --manifest-path "$FERROS_DEMO/Cargo.toml"
```

## Launch, break, inspect and step

Write the commands to a batch file. They are the same commands accepted at the
interactive `(ferros)` prompt. A fully qualified function name avoids accidentally
matching a different project's `main`.

```sh
cat > "$FERROS_DEMO/session.ferros" <<'COMMANDS'
break ferros_walkthrough::main
continue
bt
next
step
bt
finish
next
continue
quit
COMMANDS
cargo run -p ferros -- launch --batch "$FERROS_DEMO/session.ferros" \
  "$FERROS_DEMO/target/debug/ferros_walkthrough"
```

The session starts suspended at the executable's entry. `break` resolves `main`
after its prologue; `continue` reaches its first statement (line 8). `bt` prints
the current stack. `next` (`n`) reaches the call on line 9 without entering helpers.
`step` (`s`) enters `add_one` after its prologue, at line 3. The second `bt` shows
`add_one` above `main`. `finish` returns to `main` at the instruction following the
call. `next` reaches the print on line 10. `continue` prints `result: 41` and reports
`process exited with status 0`. `quit` ends the debugger.

Addresses, process/thread IDs, runtime frame names and stack lengths vary by build
and OS. Source lines 8, 9, 3 and 10 identify the expected source stops. Backtraces
can include runtime frames with unknown names. `break main.rs:9` is an alternative
source-line breakpoint; `breakpoints`, `disable <id>`, `enable <id>` and
`delete <id>` inspect or manage it. `stepi` (`si`) executes one machine instruction.

All stepping commands hold other threads suspended, including while running over
a call or back to a caller. A call that needs a suspended thread can block; Ctrl-C
interrupts it. A user breakpoint, signal or other exception ends the current step;
a process exit is reported as an exit. `finish` distinguishes recursive frames by
their stack pointer, even when their return addresses are identical.

For an interactive session, omit `--batch` and type the same commands at the prompt.
