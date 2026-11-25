//! The `ferros` command-line front end, run as a real signed binary against fixtures
//! (issue #36).
//!
//! The binary is built and signed by `ferros_tests::transcript`. Batch runs pass commands
//! with `--command`; interactive runs drive `ferros` over pipes, one prompt at a time.
#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::time::Duration;

use ferros_core::Address;
use ferros_tests::harness::{self, TestContext};
use ferros_tests::symbols::{function_address, marker_address};
use ferros_tests::transcript::{self, Interactive, normalize};
use ferros_tests::{Fixture, fixture, scratch};

/// Iterations passed to `loop_counter` so it outlives the test (10 ms each).
const LONG_LOOP: &str = "100000";

/// Runs `body` under [`harness::run`] after `ferros` is built and signed.
///
/// The first [`transcript::ferros_bin`] call builds the `ferros` package, which
/// `cargo test -p ferros-tests` does not build. On a cold CI runner that takes longer
/// than the harness deadline, so it must happen before the deadline starts.
fn run<F>(body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    let _ = transcript::ferros_bin();
    harness::run(body);
}

/// Offset of an address from the load address of `fixture`'s main image, computed with a
/// short-lived harness session (the slide differs between launches, the offset does not).
fn offset_in_image(t: &TestContext, fixture: &Fixture, address: impl Fn(&mut ferros_core::Session) -> Address) -> u64
{
    let mut d = t.launch(fixture);
    let images = d.images().expect("images");
    let main = images
        .iter()
        .find(|image| image.path == fixture.path())
        .expect("the main executable image")
        .load_address;
    address(&mut d).value() - main.value()
}

/// The load address `ferros images` prints for `fixture`'s main executable.
fn load_address(images_output: &str, fixture: &Fixture) -> u64
{
    let path = fixture.path().display().to_string();
    let line = images_output
        .lines()
        .find(|line| line.split_once(' ').is_some_and(|(_, image)| image == path))
        .unwrap_or_else(|| panic!("no image line for {path} in:\n{images_output}"));
    let hex = line
        .split(' ')
        .next()
        .and_then(|text| text.strip_prefix("0x"))
        .expect("hex load address");
    u64::from_str_radix(hex, 16).expect("load address")
}

/// The pid in `launched process <pid>`.
fn launched_pid(banner: &str) -> u32
{
    banner
        .lines()
        .find_map(|line| line.strip_prefix("launched process "))
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no 'launched process' line in {banner:?}"))
}

/// `ferros launch <hello> --command continue` shows the program's output and its exit,
/// and ferros exits 0.
#[test]
fn launch_and_continue_runs_hello_to_exit()
{
    run(|t| {
        let tag = t.tag();
        let run = transcript::launch_batch(&tag, &fixture!("hello"), &[], &["continue"]);
        assert!(run.status.success(), "status {}: {run:?}", run.status);
        assert_eq!(
            normalize(&run.stdout),
            "launched process <pid>\n(ferros) continue\nhello from ferros fixture\nprocess exited with status 0\n",
            "{run:?}"
        );
        assert_eq!(run.stderr, "", "{run:?}");
        assert_eq!(harness::tagged_pids(tag.as_str()), Vec::<u32>::new());
    });
}

/// `break *<address>` at the `hello_print` marker, then `continue` stops there and
/// `registers` shows the pc at exactly that address.
#[test]
fn breakpoint_stop_shows_pc_at_the_breakpoint()
{
    run(|t| {
        let hello = fixture!("hello");
        let offset = offset_in_image(t, &hello, |d| marker_address(d, &hello, "hello_print"));

        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &hello, &[]);
        let base = load_address(&ferros.send("images"), &hello);
        let address = base + offset;

        let reply = ferros.send(&format!("break *{address:#x}"));
        // An address breakpoint also names its function and source line (issue #37).
        let line = hello.marker_line("hello_print");
        assert!(reply.starts_with(&format!("breakpoint 1 at {address:#x}: ")), "{reply:?}");
        assert!(reply.ends_with(&format!("/fixtures/hello.rs:{line}\n")), "{reply:?}");
        let reply = ferros.send("continue");
        assert!(
            reply.starts_with(&format!("stopped: breakpoint 1 at {address:#x} (thread ")),
            "{reply:?}"
        );
        let registers = ferros.send("registers");
        let pc = registers.lines().next().unwrap_or_default();
        assert_eq!(pc, format!("pc   {address:#018x}"), "{registers}");

        let reply = ferros.send("continue");
        assert!(reply.contains("hello from ferros fixture\n"), "{reply:?}");
        assert!(reply.ends_with("process exited with status 0\n"), "{reply:?}");
        let (status, _) = ferros.finish();
        assert!(status.success(), "{status}");
    });
}

/// `step`, `finish` and `next` print where they stopped: the function and `file:line`
/// below the `stopped: step` line, as a breakpoint stop does (issue #39).
#[test]
fn source_steps_name_the_line_they_stop_on()
{
    run(|t| {
        let tag = t.tag();
        let fixture = fixture!("nested_calls");
        let file = |marker: &str| format!("/fixtures/nested_calls.rs:{}\n", fixture.marker_line(marker));
        let break_at = format!("break nested_calls.rs:{}", fixture.marker_line("call_a"));
        let commands = vec![break_at.as_str(), "continue", "step", "finish", "next"];
        let run = transcript::launch_batch(&tag, &fixture, &[], &commands);
        assert!(run.status.success(), "status {}: {run:?}", run.status);
        let replies: Vec<&str> = run.stdout.split("(ferros) ").collect();
        let reply = |command: &str| {
            replies
                .iter()
                .find(|reply| reply.starts_with(&format!("{command}\n")))
                .unwrap_or_else(|| panic!("no reply to {command}: {run:?}"))
        };
        // Each reply is `stopped: step at <pc> (thread <id>)`, the function and `file:line`,
        // and the source line itself. Check the whole location line and the whole source
        // line, not a substring of either.
        let check = |command: &str, function: &str, marker: &str, source: &str| {
            // The last reply is followed by ferros's `killed process` line.
            let lines: Vec<&str> = reply(command).lines().take(4).collect();
            let [_echo, header, location, text] = lines[..] else {
                panic!("expected the echo and three lines for `{command}`, got {lines:?}");
            };
            assert!(header.starts_with("stopped: step at 0x"), "{command}: {header:?}");
            let expected = format!("  ferros_fixture_nested_calls::{function} at ");
            assert!(
                location
                    .strip_prefix(expected.as_str())
                    .is_some_and(|path| path.ends_with(file(marker).trim_end())),
                "{command}: {location:?}"
            );
            let line = fixture.marker_line(marker).to_string();
            let shown = text
                .trim_start()
                .strip_prefix(line.as_str())
                .unwrap_or_else(|| panic!("{command}: source line is not line {line}: {text:?}"));
            assert_eq!(shown.trim(), source, "{command}: {text:?}");
        };
        check(
            "step",
            "level_a",
            "in_a",
            "let input = std::hint::black_box(val); // BREAK: in_a",
        );
        check("finish", "main", "call_a", "let res = level_a(40); // BREAK: call_a");
        check(
            "next",
            "main",
            "after_calls",
            "let final_res = std::hint::black_box(res); // BREAK: after_calls",
        );
        assert_eq!(harness::tagged_pids(tag.as_str()), Vec::<u32>::new());
    });
}

/// An unknown command prints an error on stderr, the next command still runs, and ferros
/// exits 1.
#[test]
fn unknown_command_is_an_error_and_the_session_continues()
{
    run(|t| {
        let tag = t.tag();
        let run = transcript::launch_batch(&tag, &fixture!("hello"), &[], &["frobnicate", "status"]);
        assert_eq!(run.status.code(), Some(1), "{run:?}");
        assert_eq!(run.stderr, "error: unknown command 'frobnicate' (try 'help')\n", "{run:?}");
        assert_eq!(
            normalize(&run.stdout),
            "launched process <pid>\n(ferros) frobnicate\n(ferros) status\nstatus: stopped (pid <pid>, thread <tid>)\n  at \
             entry\nkilled process <pid>\n",
            "{run:?}"
        );
    });
}

/// End of input with `loop_counter` stopped at a breakpoint: ferros kills it and exits,
/// and no process carrying the test's tag remains.
#[test]
fn end_of_input_kills_the_launched_debuggee()
{
    run(|t| {
        let fixture = fixture!("loop_counter");
        let offset = offset_in_image(t, &fixture, |d| function_address(d, &fixture, "tick"));

        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &fixture, &[LONG_LOOP]);
        let pid = launched_pid(ferros.banner());
        let base = load_address(&ferros.send("images"), &fixture);
        let reply = ferros.send(&format!("break *{:#x}", base + offset));
        assert!(reply.starts_with("breakpoint 1 at "), "{reply:?}");
        let reply = ferros.send("continue");
        assert!(reply.starts_with("stopped: breakpoint 1 at "), "{reply:?}");

        let (status, rest) = ferros.finish();
        assert!(status.success(), "{status}: {rest:?}");
        assert_eq!(normalize(&rest), "\nkilled process <pid>\n");
        assert!(
            harness::wait_until_gone(pid, Duration::from_secs(5)),
            "pid {pid} is still alive"
        );
        assert_eq!(harness::tagged_pids(tag.as_str()), Vec::<u32>::new());
    });
}

/// SIGINT to ferros while the debuggee runs stops the debuggee (ferros survives), and
/// the session keeps answering.
#[test]
fn sigint_interrupts_the_running_debuggee()
{
    run(|t| {
        let fixture = fixture!("loop_counter");
        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &fixture, &[LONG_LOOP, "--progress"]);
        let pid = launched_pid(ferros.banner());

        ferros.write_line("continue");
        assert!(
            ferros.wait_for_stdout("tick: ", Duration::from_secs(10)),
            "the debuggee never ran"
        );
        let killed = Command::new("/bin/kill")
            .args(["-INT", &ferros.pid().to_string()])
            .stdin(Stdio::null())
            .status()
            .expect("run /bin/kill");
        assert!(killed.success());
        let reply = ferros.read_reply();
        assert!(reply.contains("stopped: interrupted"), "{reply:?}");

        let reply = ferros.send("status");
        assert!(
            reply.starts_with(&format!("status: stopped (pid {pid}, thread ")),
            "{reply:?}"
        );
        assert_eq!(ferros.stderr(), "");
        let (status, rest) = ferros.finish();
        assert!(status.success(), "{status}: {rest:?}");
        assert!(
            harness::wait_until_gone(pid, Duration::from_secs(5)),
            "pid {pid} is still alive"
        );
    });
}

/// `--batch` runs the file's commands (skipping comments and blank lines), then the
/// `--command`s.
#[test]
fn batch_file_runs_before_commands()
{
    run(|t| {
        let dir = scratch::create_dir("ferros-cli-batch");
        let script = dir.join("script.ferros");
        std::fs::write(&script, "# set up\n\nstatus\ncontinue\n").expect("write script");

        let tag = t.tag();
        let mut child = Command::new(transcript::ferros_bin());
        child
            .args(["launch", "--batch"])
            .arg(&script)
            .args(["--command", "status"])
            .arg(fixture!("hello").path())
            .args(["--", tag.as_str()])
            .stdin(Stdio::null());
        let output = child.output().expect("run ferros");
        let _ = std::fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            normalize(&stdout),
            "launched process <pid>\n(ferros) status\nstatus: stopped (pid <pid>, thread <tid>)\n  at entry\n(ferros) \
             continue\nhello from ferros fixture\nprocess exited with status 0\n(ferros) status\nstatus: exited\n  process \
             exited with status 0\n"
        );
    });
}

/// A temporary directory removed when dropped.
struct TempDir(std::path::PathBuf);

impl TempDir
{
    fn new(name: &str) -> Self
    {
        Self(scratch::create_dir(&format!("ferros-cli-{name}")))
    }

    /// Write `contents` to `name` in the directory and return its path.
    fn file(&self, name: &str, contents: &str) -> std::path::PathBuf
    {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir
{
    fn drop(&mut self)
    {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pipe mode (issue #95): the launched program reads `/dev/null`, not ferros's stdin. Both
/// commands are written before the first reply, so a program sharing the pipe would take
/// input meant for ferros or block reading it; instead ferros answers both and `read_line`
/// sees end of file.
#[test]
fn pipe_mode_gives_the_program_an_empty_stdin()
{
    run(|t| {
        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &fixture!("read_line"), &[]);
        ferros.write_line("continue");
        ferros.write_line("status");
        let reply = ferros.read_reply();
        assert_eq!(reply, "read: <eof>\nprocess exited with status 0\n", "{reply:?}");
        let reply = ferros.read_reply();
        assert_eq!(reply, "status: exited\n  process exited with status 0\n", "{reply:?}");
        assert_eq!(ferros.stderr(), "");
        let (status, rest) = ferros.finish();
        assert!(status.success(), "{status}: {rest:?}");
    });
}

/// `launch --stdin <file>` feeds the file to the program in pipe mode, while ferros keeps
/// reading its commands from the pipe.
#[test]
fn stdin_option_feeds_the_file_to_the_program_in_pipe_mode()
{
    run(|t| {
        let dir = TempDir::new("stdin-pipe");
        let input = dir.file("input.txt", "line from the file\nsecond line\n");
        let input = input.display().to_string();

        let tag = t.tag();
        let mut ferros = Interactive::launch_with_options(&tag, &fixture!("read_line"), &["--stdin", &input], &[]);
        let reply = ferros.send("continue");
        assert_eq!(reply, "read: line from the file\nprocess exited with status 0\n", "{reply:?}");
        assert_eq!(ferros.stderr(), "");
        let (status, rest) = ferros.finish();
        assert!(status.success(), "{status}: {rest:?}");
    });
}

/// `launch --stdin <file>` also overrides batch mode's inherited stdin, and `run`
/// relaunches the program with the file opened again from its start.
#[test]
fn stdin_option_overrides_batch_stdin_and_is_reopened_by_run()
{
    run(|t| {
        let dir = TempDir::new("stdin-batch");
        let input = dir.file("input.txt", "line from the file\n");
        let input = input.display().to_string();

        let tag = t.tag();
        let run = transcript::launch_batch_with(
            &tag,
            &fixture!("read_line"),
            &["--stdin", &input],
            &[],
            &["continue", "run", "continue"],
            Some(b"line from ferros stdin\n"),
        );
        assert!(run.status.success(), "status {}: {run:?}", run.status);
        assert_eq!(
            normalize(&run.stdout),
            "launched process <pid>\n(ferros) continue\nread: line from the file\nprocess exited with status 0\n(ferros) \
             run\nlaunched process <pid> (stopped at entry; address breakpoints were cleared)\n(ferros) continue\nread: \
             line from the file\nprocess exited with status 0\n",
            "{run:?}"
        );
        assert_eq!(run.stderr, "", "{run:?}");
    });
}

/// A `--stdin` file that cannot be opened is reported before anything is launched.
#[test]
fn stdin_option_with_a_missing_file_fails_before_launching()
{
    run(|t| {
        let dir = TempDir::new("stdin-missing");
        let missing = dir.0.join("missing.txt").display().to_string();

        let tag = t.tag();
        let run =
            transcript::launch_batch_with(&tag, &fixture!("read_line"), &["--stdin", &missing], &[], &["continue"], None);
        assert_eq!(run.status.code(), Some(1), "{run:?}");
        assert_eq!(run.stdout, "", "{run:?}");
        assert!(run.stderr.starts_with(&format!("error: {missing}: ")), "{run:?}");
        assert_eq!(harness::tagged_pids(tag.as_str()), Vec::<u32>::new());
    });
}

/// Batch mode is unchanged by issue #95: ferros does not read stdin, so the program
/// inherits it and reads what was piped into ferros.
#[test]
fn batch_mode_program_inherits_stdin()
{
    run(|t| {
        let tag = t.tag();
        let run = transcript::launch_batch_with(
            &tag,
            &fixture!("read_line"),
            &[],
            &[],
            &["continue"],
            Some(b"line from ferros stdin\n"),
        );
        assert!(run.status.success(), "status {}: {run:?}", run.status);
        assert_eq!(
            normalize(&run.stdout),
            "launched process <pid>\n(ferros) continue\nread: line from ferros stdin\nprocess exited with status 0\n",
            "{run:?}"
        );
        assert_eq!(run.stderr, "", "{run:?}");
    });
}

/// `ferros --help` lists only the subcommands that work. Launches no fixture process, so
/// it does not need the harness.
#[test]
fn help_lists_no_stub_subcommands()
{
    let output = Command::new(transcript::ferros_bin())
        .arg("--help")
        .output()
        .expect("run ferros --help");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    let commands: Vec<&str> = help
        .split("Commands:")
        .nth(1)
        .and_then(|rest| rest.split("Options:").next())
        .expect("a Commands section")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(commands.join(" "), "launch attach find-logs help", "{help}");
}
