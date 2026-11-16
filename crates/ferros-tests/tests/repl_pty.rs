//! The interactive (rustyline) REPL, driven through a pseudo-terminal (issue #96).
//!
//! `ferros` picks the line editor only when stdin is a terminal, so these tests start the
//! signed binary from `ferros_tests::transcript` on the slave side of a pty (as its
//! controlling terminal) and type into the master side. Every run gets a temporary `HOME`
//! and a `FERROS_HISTORY` inside it, so the user's `~/.ferros_history` and `~/.ferros`
//! logs are never touched.
//!
//! rustyline reads in raw mode with `ISIG` off: Ctrl-C (0x03) and Ctrl-D (0x04) typed at
//! the prompt arrive as keys, not as `SIGINT` or a tty end of file.
#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};
use std::{fs, thread};

use ferros_tests::harness::{self, Tag, TestContext};
use ferros_tests::transcript::{self, PROMPT};
use ferros_tests::{Fixture, fixture};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

/// How long one command may take to print what the test waits for.
const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// Ctrl-C and Ctrl-D as typed at a terminal.
const CTRL_C: &[u8] = b"\x03";
const CTRL_D: &[u8] = b"\x04";

/// Runs `body` under [`harness::run`] after `ferros` is built and signed (the build can
/// outlast the harness deadline on a cold runner).
fn run<F>(body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    let _ = transcript::ferros_bin();
    harness::run(body);
}

/// A temporary directory standing in for `HOME`, removed on drop.
struct Scratch(PathBuf);

impl Scratch
{
    fn new() -> Self
    {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferros-repl-pty-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create scratch HOME");
        Self(fs::canonicalize(&path).expect("canonical scratch HOME"))
    }

    fn home(&self) -> &Path
    {
        &self.0
    }

    /// The history file passed in `FERROS_HISTORY` (outside `HOME`'s default location).
    fn history(&self) -> PathBuf
    {
        self.0.join("history-override")
    }
}

impl Drop for Scratch
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Bytes read from the pty master so far, and whether the reader saw the end.
type Screen = Arc<(Mutex<(Vec<u8>, bool)>, Condvar)>;

/// `ferros launch <fixture> -- <tag>` on a pseudo-terminal.
///
/// A background thread copies the master's output into a buffer; it is never joined, so a
/// read that does not end (a leaked writer on the slave) cannot hang the test. Dropping
/// the value kills and reaps `ferros` if it is still running.
struct PtyFerros
{
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Screen,
    /// Kept open so the slave stays usable until the test is done with it.
    _master: Box<dyn MasterPty + Send>,
    /// Byte offset of the output already consumed by [`PtyFerros::expect`].
    cursor: usize,
}

impl PtyFerros
{
    /// Start `ferros` with `HOME` and `FERROS_HISTORY` inside `scratch` and wait for its
    /// first prompt.
    fn launch(tag: &Tag, fixture: &Fixture, scratch: &Scratch) -> Self
    {
        Self::launch_with(tag, fixture, scratch, &[], &[])
    }

    /// [`PtyFerros::launch`] with `ferros launch` `options` (before the program) and
    /// program `args` (before the tag).
    fn launch_with(tag: &Tag, fixture: &Fixture, scratch: &Scratch, options: &[&str], args: &[&str]) -> Self
    {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty");
        let mut command = CommandBuilder::new(transcript::ferros_bin());
        command.arg("launch");
        for option in options {
            command.arg(option);
        }
        command.arg(fixture.path());
        command.arg("--");
        for arg in args {
            command.arg(arg);
        }
        command.arg(tag.as_str());
        command.cwd(scratch.home());
        command.env("HOME", scratch.home());
        command.env("FERROS_HISTORY", scratch.history());
        // rustyline falls back to plain line reading on `dumb` and unknown terminals.
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).expect("spawn ferros on the pty");
        // Only the child holds the slave now, so the master sees the end once it exits.
        drop(pair.slave);

        let screen: Screen = Arc::default();
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let sink = Arc::clone(&screen);
        thread::spawn(move || {
            let mut chunk = vec![0u8; 4096];
            loop {
                let read = reader.read(&mut chunk).unwrap_or(0);
                let (lock, ready) = &*sink;
                let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
                if read == 0 {
                    state.1 = true;
                    ready.notify_all();
                    return;
                }
                state.0.extend_from_slice(&chunk[..read]);
                ready.notify_all();
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        let mut ferros = Self {
            child,
            writer,
            screen,
            _master: pair.master,
            cursor: 0,
        };
        ferros.expect("launched process ");
        ferros.expect(PROMPT);
        ferros
    }

    /// Everything read from the terminal so far.
    fn text(&self) -> String
    {
        let state = self.screen.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&state.0).into_owned()
    }

    /// Wait until `needle` appears after the cursor and move the cursor past it.
    fn expect(&mut self, needle: &str)
    {
        let deadline = Instant::now() + STEP_TIMEOUT;
        let (lock, ready) = &*self.screen;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(found) = state
                .0
                .get(self.cursor..)
                .and_then(|rest| rest.windows(needle.len()).position(|window| window == needle.as_bytes()))
            {
                self.cursor += found + needle.len();
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if state.1 || remaining.is_zero() {
                let text = String::from_utf8_lossy(&state.0).into_owned();
                drop(state);
                panic!("no {needle:?} on the terminal within {STEP_TIMEOUT:?}; terminal output:\n{text}");
            }
            state = ready.wait_timeout(state, remaining).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Type `bytes` as keys.
    fn type_keys(&mut self, bytes: &[u8])
    {
        self.writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush())
            .unwrap_or_else(|err| panic!("write {bytes:?} to the pty: {err}"));
    }

    /// Type `line` and Enter.
    fn type_line(&mut self, line: &str)
    {
        self.type_keys(format!("{line}\r").as_bytes());
    }

    /// Whether `ferros` is still running.
    fn is_running(&mut self) -> bool
    {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait for `ferros` to exit and return its status.
    fn wait_exit(&mut self) -> ExitStatus
    {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "ferros did not exit within {STEP_TIMEOUT:?}; terminal output:\n{}",
                self.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for PtyFerros
{
    fn drop(&mut self)
    {
        if self.is_running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Every process still carrying `tag` (the fixture and `ferros`).
fn tagged(tag: &Tag) -> Vec<u32>
{
    harness::tagged_pids(tag.as_str())
}

#[test]
fn continue_then_ctrl_d_ends_the_session()
{
    run(|t| {
        let tag = t.tag();
        let scratch = Scratch::new();
        let mut ferros = PtyFerros::launch(&tag, &fixture!("hello"), &scratch);

        ferros.type_line("continue");
        ferros.expect("hello from ferros fixture");
        ferros.expect("process exited with status 0");
        ferros.expect(PROMPT);

        ferros.type_keys(CTRL_D);
        let status = ferros.wait_exit();
        assert!(
            status.success(),
            "ferros exited with {status:?}; terminal output:\n{}",
            ferros.text()
        );
        assert!(tagged(&tag).is_empty(), "processes left running: {:?}", tagged(&tag));
    });
}

#[test]
fn ctrl_c_at_the_prompt_discards_the_line_and_keeps_the_session()
{
    run(|t| {
        let tag = t.tag();
        let scratch = Scratch::new();
        let mut ferros = PtyFerros::launch(&tag, &fixture!("hello"), &scratch);

        // A half-typed line, then Ctrl-C: the line is dropped and a fresh prompt appears.
        ferros.type_keys(b"bogus");
        ferros.expect("bogus");
        ferros.type_keys(CTRL_C);
        ferros.expect(PROMPT);
        thread::sleep(Duration::from_millis(200));
        assert!(ferros.is_running(), "ferros died on Ctrl-C:\n{}", ferros.text());

        // Ctrl-C on an empty line too.
        ferros.type_keys(CTRL_C);
        ferros.expect(PROMPT);

        ferros.type_line("help");
        ferros.expect("list the commands");
        ferros.expect(PROMPT);

        // The debuggee survived and still runs to completion.
        ferros.type_line("continue");
        ferros.expect("hello from ferros fixture");
        ferros.expect("process exited with status 0");
        ferros.expect(PROMPT);

        ferros.type_keys(CTRL_D);
        let status = ferros.wait_exit();
        let text = ferros.text();
        assert!(status.success(), "ferros exited with {status:?}; terminal output:\n{text}");
        assert!(!text.contains("unknown command 'bogus"), "the interrupted line ran:\n{text}");
        assert!(tagged(&tag).is_empty(), "processes left running: {:?}", tagged(&tag));
    });
}

#[test]
fn history_goes_to_ferros_history_not_home()
{
    run(|t| {
        let tag = t.tag();
        let scratch = Scratch::new();
        let history = scratch.history();
        let default_history = scratch.home().join(".ferros_history");
        fs::write(&history, "earlier-command\n").expect("seed the history file");

        let mut ferros = PtyFerros::launch(&tag, &fixture!("hello"), &scratch);
        // Up-arrow recalls the seeded entry: the history was loaded from the override.
        ferros.type_keys(b"\x1b[A");
        ferros.expect("earlier-command");
        ferros.type_keys(CTRL_C);
        ferros.expect(PROMPT);

        ferros.type_line("status");
        ferros.expect("status: stopped");
        ferros.expect(PROMPT);
        ferros.type_keys(CTRL_D);
        let status = ferros.wait_exit();
        assert!(
            status.success(),
            "ferros exited with {status:?}; terminal output:\n{}",
            ferros.text()
        );

        let saved = fs::read_to_string(&history).expect("read the history file");
        let entries: Vec<&str> = saved.lines().filter(|line| !line.starts_with('#')).collect();
        assert!(
            entries.contains(&"earlier-command"),
            "history lost the seeded entry:\n{saved}"
        );
        assert!(entries.contains(&"status"), "history is missing `status`:\n{saved}");
        assert!(
            !default_history.exists(),
            "history was also written to {}",
            default_history.display()
        );
        assert!(tagged(&tag).is_empty(), "processes left running: {:?}", tagged(&tag));
    });
}

/// Type Ctrl-D at the prompt and check that `ferros` exits 0 and leaves nothing behind.
fn end_with_ctrl_d(ferros: &mut PtyFerros, tag: &Tag)
{
    ferros.type_keys(CTRL_D);
    let status = ferros.wait_exit();
    assert!(
        status.success(),
        "ferros exited with {status:?}; terminal output:\n{}",
        ferros.text()
    );
    assert!(tagged(tag).is_empty(), "processes left running: {:?}", tagged(tag));
}

/// In terminal mode the program reads the terminal: rustyline gives the terminal back
/// (cooked mode, echo on) while the program runs, so a typed line reaches it.
#[test]
fn the_program_reads_a_line_typed_at_the_terminal()
{
    run(|t| {
        let tag = t.tag();
        let scratch = Scratch::new();
        let mut ferros = PtyFerros::launch(&tag, &fixture!("read_line"), &scratch);

        ferros.type_line("continue");
        ferros.expect("continue");
        // Let the program block in its read; a line typed earlier would also be queued.
        thread::sleep(Duration::from_millis(300));
        ferros.type_line("hello from tty");
        ferros.expect("read: hello from tty");
        ferros.expect("process exited with status 0");
        ferros.expect(PROMPT);

        end_with_ctrl_d(&mut ferros, &tag);
    });
}

/// `launch --stdin FILE` in terminal mode: the program reads the file, not the terminal.
#[test]
fn stdin_file_wins_over_the_terminal()
{
    run(|t| {
        let tag = t.tag();
        let scratch = Scratch::new();
        let input = scratch.home().join("input.txt");
        fs::write(&input, "from the file\n").expect("write the stdin file");
        let input = input.to_str().expect("utf-8 path");
        let mut ferros = PtyFerros::launch_with(&tag, &fixture!("read_line"), &scratch, &["--stdin", input], &[]);

        ferros.type_line("continue");
        ferros.expect("read: from the file");
        ferros.expect("process exited with status 0");
        ferros.expect(PROMPT);

        end_with_ctrl_d(&mut ferros, &tag);
        assert!(
            !ferros.text().contains("read: hello"),
            "the program read the terminal:\n{}",
            ferros.text()
        );
    });
}

/// Ctrl-C typed while the program runs stops it and returns to the prompt.
///
/// The terminal is in cooked mode with `ISIG` on while the program runs, and the program
/// shares ferros's process group (it is spawned without `POSIX_SPAWN_SETPGROUP`), so the
/// terminal sends `SIGINT` to both. ferros survives and interrupts the program; the stop
/// is reported as `stopped: interrupted` or, when the program's own `SIGINT` is seen
/// first, `stopped: signal 2` (see `crates/ferros/src/repl/interrupt.rs`).
#[test]
fn ctrl_c_while_the_program_runs_stops_it()
{
    run(|t| {
        let tag = t.tag();
        let scratch = Scratch::new();
        let mut ferros = PtyFerros::launch_with(&tag, &fixture!("loop_counter"), &scratch, &[], &["100000", "--progress"]);

        ferros.type_line("continue");
        ferros.expect("tick: ");
        ferros.type_keys(CTRL_C);
        ferros.expect("stopped: ");
        ferros.expect(PROMPT);
        thread::sleep(Duration::from_millis(200));
        assert!(ferros.is_running(), "ferros died on Ctrl-C:\n{}", ferros.text());
        let text = ferros.text();
        assert!(
            text.contains("stopped: interrupted") || text.contains("stopped: signal 2 "),
            "not an interrupt stop:\n{text}"
        );
        assert!(!text.contains("process killed"), "the program died:\n{text}");

        ferros.type_line("status");
        ferros.expect("status: stopped (pid ");
        ferros.expect(PROMPT);
        ferros.type_line("backtrace");
        ferros.expect("#0 ");
        ferros.expect(PROMPT);

        // `continue` does not pass the `SIGINT` on: the program keeps counting, and a
        // second Ctrl-C stops it again.
        ferros.type_line("continue");
        ferros.expect("tick: ");
        thread::sleep(Duration::from_millis(100));
        ferros.type_keys(CTRL_C);
        ferros.expect("stopped: ");
        ferros.expect(PROMPT);
        let text = ferros.text();
        assert!(!text.contains("process killed"), "the program died:\n{text}");
        assert!(!text.contains("process exited"), "the program ended:\n{text}");

        end_with_ctrl_d(&mut ferros, &tag);
        assert!(ferros.text().contains("killed process "), "{}", ferros.text());
    });
}
