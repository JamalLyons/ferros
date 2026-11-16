//! Drive the real `ferros` binary against fixtures and check what it prints (macOS only).
//!
//! The `ferros` executable belongs to another package, so `CARGO_BIN_EXE_ferros` does not
//! exist here. [`ferros_bin`] builds it once per test process with
//! `$CARGO build -p ferros --message-format=json`, takes the executable path from cargo's
//! `compiler-artifact` message, copies it next to itself and ad-hoc signs the copy with
//! the debugger entitlement (`crates/ferros-macos/ferros.entitlements`). Set `FERROS_BIN`
//! to use an already signed binary instead.
//!
//! Every fixture launched through these helpers receives the identifying argument of a
//! [`Tag`] after `--`, so the harness kills it (and the `ferros` process, whose command line
//! carries the same argument) if the test fails or times out.
//!
//! ```ignore
//! use ferros_tests::transcript::{self, Interactive};
//! use ferros_tests::{fixture, harness};
//!
//! harness::run(|t| {
//!     let tag = t.tag();
//!     let run = transcript::launch_batch(&tag, &fixture!("hello"), &[], &["continue"]);
//!     assert!(run.status.success());
//!
//!     let mut ferros = Interactive::launch(&t.tag(), &fixture!("hello"), &[]);
//!     let reply = ferros.send("status");
//!     assert!(reply.starts_with("status: stopped"));
//! });
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::Fixture;
use crate::harness::Tag;

/// The prompt `ferros` prints before reading each command from a pipe.
pub const PROMPT: &str = "(ferros) ";

/// How long a batch run, or one interactive command, may take.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// How long dropping an [`Interactive`] waits for `ferros` to exit after closing its stdin.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Path of a `ferros` binary signed with the debugger entitlement.
///
/// `FERROS_BIN`, when set, is returned as is. Otherwise the binary is built and signed on
/// the first call (see the [module documentation](self)); later calls return the same path.
///
/// # Panics
///
/// Panics when the build, the copy or the signing fails, with cargo's or codesign's output.
pub fn ferros_bin() -> &'static Path
{
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(path) = std::env::var_os("FERROS_BIN") {
            return PathBuf::from(path);
        }
        sign_copy(&build_ferros())
    })
}

/// The repository root: two levels above this crate's manifest directory.
fn repo_root() -> PathBuf
{
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("no repository root above {}", manifest.display()))
        .to_path_buf()
}

/// `cargo` arguments that build the `ferros` binary and report its path.
const BUILD_ARGS: &[&str] = &["build", "--package=ferros", "--bin=ferros", "--message-format=json"];

/// Build the `ferros` package and return its executable path.
fn build_ferros() -> PathBuf
{
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
        .args(BUILD_ARGS)
        .current_dir(repo_root())
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|err| panic!("ferros-tests: cannot run {}: {err}", cargo.to_string_lossy()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ferros-tests: `cargo build -p ferros` failed ({}):\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
        .lines()
        .filter(|line| line.contains(r#""reason":"compiler-artifact""#))
        .filter(|line| line.contains(r#""kind":["bin"]"#) && line.contains(r#""name":"ferros""#))
        .find_map(|line| json_string_field(line, "executable"))
        .map_or_else(
            || panic!("ferros-tests: no executable for the ferros binary in cargo's output:\n{stdout}"),
            PathBuf::from,
        )
}

/// The string value of `"field":"..."` in one line of JSON (with `\\`, `\"` and `\/` unescaped).
fn json_string_field(line: &str, field: &str) -> Option<String>
{
    let key = format!("\"{field}\":\"");
    let start = line.find(&key)? + key.len();
    let mut value = String::new();
    let mut chars = line[start..].chars();
    loop {
        match chars.next()? {
            '"' => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                other => value.push(other),
            },
            other => value.push(other),
        }
    }
}

/// Copy `exe` to a sibling `ferros-tests-signed` and ad-hoc sign the copy with the
/// debugger entitlement. The copy is signed under a per-process name and renamed into
/// place, so concurrent test processes never run a half-written file.
fn sign_copy(exe: &Path) -> PathBuf
{
    let entitlements = repo_root().join("crates/ferros-macos/ferros.entitlements");
    let target = exe.with_file_name("ferros-tests-signed");
    let staging = exe.with_file_name(format!("ferros-tests-signed.{}.tmp", std::process::id()));
    std::fs::copy(exe, &staging)
        .unwrap_or_else(|err| panic!("ferros-tests: copy {} to {}: {err}", exe.display(), staging.display()));
    let output = Command::new("codesign")
        .arg("--entitlements")
        .arg(&entitlements)
        .args(["--force", "--sign", "-"])
        .arg(&staging)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|err| panic!("ferros-tests: cannot run codesign: {err}"));
    if !output.status.success() {
        let _ = std::fs::remove_file(&staging);
        panic!(
            "ferros-tests: codesign of {} failed ({}):\n{}",
            staging.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::rename(&staging, &target)
        .unwrap_or_else(|err| panic!("ferros-tests: rename {} to {}: {err}", staging.display(), target.display()));
    target
}

/// The `ferros launch` arguments for `fixture`: `options` (such as `--command`), then the
/// program, then `--`, the fixture's `args` and the tag.
fn launch_args(tag: &Tag, fixture: &Fixture, options: &[String], args: &[&str]) -> Vec<String>
{
    let mut command_line = vec!["launch".to_owned()];
    command_line.extend(options.iter().cloned());
    command_line.push(fixture.path().display().to_string());
    command_line.push("--".to_owned());
    command_line.extend(args.iter().map(|arg| (*arg).to_owned()));
    command_line.push(tag.as_str().to_owned());
    command_line
}

/// The bytes read so far and whether the pipe has closed.
type CaptureState = Mutex<(Vec<u8>, bool)>;

/// Output collected from one pipe by a background thread.
#[derive(Clone, Default)]
struct Captured
{
    shared: Arc<(CaptureState, Condvar)>,
}

impl Captured
{
    fn spawn(mut pipe: impl Read + Send + 'static) -> Self
    {
        let captured = Self::default();
        let sink = captured.clone();
        thread::spawn(move || {
            let mut chunk = vec![0u8; 4096];
            loop {
                let read = pipe.read(&mut chunk).unwrap_or(0);
                let (lock, ready) = &*sink.shared;
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
        captured
    }

    fn text(&self) -> String
    {
        let state = self.shared.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&state.0).into_owned()
    }

    /// Wait until `needle` appears at or after byte `from`, or the pipe closes, or the
    /// timeout passes. Returns the byte offset of the match.
    fn wait_for(&self, needle: &str, from: usize, timeout: Duration) -> Option<usize>
    {
        let deadline = Instant::now() + timeout;
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(found) = find(&state.0, needle.as_bytes(), from) {
                return Some(found);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if state.1 || remaining.is_zero() {
                return None;
            }
            state = ready.wait_timeout(state, remaining).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    fn wait_closed(&self, timeout: Duration) -> bool
    {
        let deadline = Instant::now() + timeout;
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !state.1 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = ready.wait_timeout(state, remaining).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize>
{
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

/// Wait up to `timeout` for `child` to exit.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus>
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// What a batch run of `ferros` printed and how it exited.
#[derive(Debug)]
pub struct Transcript
{
    /// Everything on stdout: ferros's lines and the debuggee's own output.
    pub stdout: String,
    /// Everything on stderr (`error: ...` lines and the debuggee's stderr).
    pub stderr: String,
    /// Exit status of `ferros`.
    pub status: ExitStatus,
}

/// Run `ferros launch --command <c>... <fixture> -- <args>... <tag>` with stdin closed and
/// wait for it to exit.
///
/// # Panics
///
/// Panics when `ferros` cannot be started or does not exit within [`COMMAND_TIMEOUT`]
/// (it is killed first, and its fixture by the tag).
#[must_use]
pub fn launch_batch(tag: &Tag, fixture: &Fixture, args: &[&str], commands: &[&str]) -> Transcript
{
    let options: Vec<String> = commands
        .iter()
        .flat_map(|command| ["--command".to_owned(), (*command).to_owned()])
        .collect();
    let mut child = Command::new(ferros_bin())
        .args(launch_args(tag, fixture, &options, args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("ferros-tests: cannot start {}: {err}", ferros_bin().display()));
    let stdout = Captured::spawn(child.stdout.take().expect("piped stdout"));
    let stderr = Captured::spawn(child.stderr.take().expect("piped stderr"));
    let Some(status) = wait_with_timeout(&mut child, COMMAND_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "ferros-tests: ferros did not exit within {COMMAND_TIMEOUT:?}\nstdout:\n{}\nstderr:\n{}",
            stdout.text(),
            stderr.text()
        );
    };
    // The pipes close once every writer (ferros and its debuggee) is gone.
    stdout.wait_closed(EXIT_TIMEOUT);
    stderr.wait_closed(EXIT_TIMEOUT);
    Transcript {
        stdout: stdout.text(),
        stderr: stderr.text(),
        status,
    }
}

/// `ferros` reading commands from a pipe, one at a time.
///
/// Dropping it closes stdin (ending the session: the debuggee is killed), waits briefly
/// for `ferros` to exit and kills it if it does not.
pub struct Interactive
{
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Captured,
    stderr: Captured,
    /// Byte offset in stdout just after the last prompt consumed.
    cursor: usize,
    /// First stdout line (`launched process <pid>`).
    banner: String,
}

impl Interactive
{
    /// Start `ferros launch <fixture> -- <args>... <tag>` and wait for its first prompt.
    ///
    /// # Panics
    ///
    /// Panics when `ferros` cannot be started or prints no prompt within
    /// [`COMMAND_TIMEOUT`].
    #[must_use]
    pub fn launch(tag: &Tag, fixture: &Fixture, args: &[&str]) -> Self
    {
        let mut child = Command::new(ferros_bin())
            .args(launch_args(tag, fixture, &[], args))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("ferros-tests: cannot start {}: {err}", ferros_bin().display()));
        let stdin = child.stdin.take();
        let stdout = Captured::spawn(child.stdout.take().expect("piped stdout"));
        let stderr = Captured::spawn(child.stderr.take().expect("piped stderr"));
        let mut session = Self {
            child,
            stdin,
            stdout,
            stderr,
            cursor: 0,
            banner: String::new(),
        };
        session.banner = session.read_reply();
        session
    }

    /// Pid of the `ferros` process.
    #[must_use]
    pub fn pid(&self) -> u32
    {
        self.child.id()
    }

    /// What `ferros` printed before its first prompt (`launched process <pid>`).
    #[must_use]
    pub fn banner(&self) -> &str
    {
        &self.banner
    }

    /// Send `command` and return what it printed on stdout, up to the next prompt,
    /// without the echoed command line.
    ///
    /// # Panics
    ///
    /// As for [`Interactive::read_reply`], or when stdin is closed.
    pub fn send(&mut self, command: &str) -> String
    {
        self.write_line(command);
        self.read_reply()
    }

    /// Send `command` without waiting for its output (see [`Interactive::read_reply`]).
    ///
    /// # Panics
    ///
    /// Panics when the line cannot be written.
    pub fn write_line(&mut self, command: &str)
    {
        let stdin = self.stdin.as_mut().expect("ferros stdin is open");
        writeln!(stdin, "{command}")
            .and_then(|()| stdin.flush())
            .unwrap_or_else(|err| panic!("ferros-tests: write {command:?} to ferros: {err}"));
    }

    /// Wait for the next prompt and return the stdout text before it, without the echoed
    /// command line.
    ///
    /// # Panics
    ///
    /// Panics when no prompt arrives within [`COMMAND_TIMEOUT`] (for example because
    /// `ferros` exited), with everything printed so far.
    pub fn read_reply(&mut self) -> String
    {
        let Some(found) = self.stdout.wait_for(PROMPT, self.cursor, COMMAND_TIMEOUT) else {
            panic!(
                "ferros-tests: no prompt from ferros within {COMMAND_TIMEOUT:?}\nstdout:\n{}\nstderr:\n{}",
                self.stdout.text(),
                self.stderr.text()
            );
        };
        let text = self.stdout.text();
        let reply = text.get(self.cursor..found).unwrap_or_default().to_owned();
        let first = self.cursor == 0;
        self.cursor = found + PROMPT.len();
        if first {
            // The first prompt follows the banner; nothing was echoed.
            return reply;
        }
        reply.split_once('\n').map_or(reply.clone(), |(_, rest)| rest.to_owned())
    }

    /// Wait up to `timeout` for `needle` to appear on stdout after the last prompt.
    #[must_use]
    pub fn wait_for_stdout(&self, needle: &str, timeout: Duration) -> bool
    {
        self.stdout.wait_for(needle, self.cursor, timeout).is_some()
    }

    /// Everything `ferros` printed on stderr so far.
    #[must_use]
    pub fn stderr(&self) -> String
    {
        self.stderr.text()
    }

    /// Close stdin (end of input), wait for `ferros` to exit and return its status and
    /// everything printed on stdout after the last prompt.
    ///
    /// # Panics
    ///
    /// Panics when `ferros` does not exit within the timeout (it is killed first).
    #[must_use = "the exit status says whether ferros succeeded"]
    pub fn finish(mut self) -> (ExitStatus, String)
    {
        drop(self.stdin.take());
        let Some(status) = wait_with_timeout(&mut self.child, COMMAND_TIMEOUT) else {
            panic!(
                "ferros-tests: ferros did not exit at the end of input\nstdout:\n{}",
                self.stdout.text()
            );
        };
        self.stdout.wait_closed(EXIT_TIMEOUT);
        let rest = self.stdout.text().get(self.cursor..).unwrap_or_default().to_owned();
        (status, rest)
    }
}

impl Drop for Interactive
{
    fn drop(&mut self)
    {
        drop(self.stdin.take());
        if wait_with_timeout(&mut self.child, EXIT_TIMEOUT).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Words followed by a number that changes from run to run, and its placeholder.
const PLACEHOLDERS: &[(&str, &str)] = &[("process ", "<pid>"), ("pid ", "<pid>"), ("thread ", "<tid>")];

/// Replace the parts of a transcript that change from run to run: hex numbers become
/// `0x<hex>`, and the number after `process`, `pid` or `thread` becomes `<pid>` or `<tid>`.
#[must_use]
pub fn normalize(text: &str) -> String
{
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("0x") {
            let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_hexdigit()).len();
            if digits > 0 {
                out.push_str("0x<hex>");
                rest = &after[digits..];
                continue;
            }
        }
        if let Some((word, placeholder)) = PLACEHOLDERS
            .iter()
            .copied()
            .find(|(word, _)| rest.starts_with(word) && !ends_in_word(&out))
        {
            let after = &rest[word.len()..];
            let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if digits > 0 {
                out.push_str(word);
                out.push_str(placeholder);
                rest = &after[digits..];
                continue;
            }
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            out.push(c);
        }
        rest = chars.as_str();
    }
    out
}

/// Whether `text` ends in the middle of a word (so a keyword match would be a suffix).
fn ends_in_word(text: &str) -> bool
{
    text.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn normalize_replaces_addresses_pids_and_threads()
    {
        let text = "launched process 4242\nstopped: breakpoint 1 at 0x1000abc (thread 8963)\nstatus: stopped (pid 77, \
                    thread 5)\npc   0x00000001000004f0\n* 0 thread 99\n";
        assert_eq!(
            normalize(text),
            "launched process <pid>\nstopped: breakpoint 1 at 0x<hex> (thread <tid>)\nstatus: stopped (pid <pid>, thread \
             <tid>)\npc   0x<hex>\n* 0 thread <tid>\n"
        );
    }

    #[test]
    fn normalize_keeps_other_numbers()
    {
        assert_eq!(normalize("process exited with status 0"), "process exited with status 0");
        assert_eq!(normalize("subprocess 12 0x"), "subprocess 12 0x");
    }

    #[test]
    fn json_field_is_unescaped()
    {
        let line = r#"{"reason":"compiler-artifact","executable":"/a\\b/\"c\"/ferros","fresh":true}"#;
        assert_eq!(json_string_field(line, "executable").as_deref(), Some(r#"/a\b/"c"/ferros"#));
        assert_eq!(json_string_field(r#"{"executable":null}"#, "executable"), None);
    }
}
