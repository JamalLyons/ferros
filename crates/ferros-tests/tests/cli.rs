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
use ferros_tests::{Fixture, fixture};

/// Iterations passed to `loop_counter` so it outlives the test (10 ms each).
const LONG_LOOP: &str = "100000";

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
    harness::run(|t| {
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
    harness::run(|t| {
        let hello = fixture!("hello");
        let offset = offset_in_image(t, &hello, |d| marker_address(d, &hello, "hello_print"));

        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &hello, &[]);
        let base = load_address(&ferros.send("images"), &hello);
        let address = base + offset;

        let reply = ferros.send(&format!("break *{address:#x}"));
        assert_eq!(reply, format!("breakpoint 1 at {address:#x}\n"));
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

/// An unknown command prints an error on stderr, the next command still runs, and ferros
/// exits 1.
#[test]
fn unknown_command_is_an_error_and_the_session_continues()
{
    harness::run(|t| {
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
    harness::run(|t| {
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
    harness::run(|t| {
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
    harness::run(|t| {
        let dir = std::env::temp_dir().join(format!("ferros-cli-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
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
