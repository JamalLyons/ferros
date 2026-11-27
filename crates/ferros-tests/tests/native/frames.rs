//! Selected stack frame through the `frame`, `up`, and `down` commands (issue #382).
#![cfg(target_os = "macos")]

use ferros_tests::fixture;
use ferros_tests::harness::{self, TestContext};
use ferros_tests::transcript::{self, Transcript};

/// `up` twice from `level_c` selects `level_a` at its call of `level_b`. `down` past
/// frame 0 is an error and leaves that frame selected. A later `thread`, `next`, and
/// `continue` each return the selection to frame 0, and `help` lists the commands.
#[test]
fn up_selects_the_grandparent_and_the_selection_resets()
{
    // Build and sign `ferros` before the harness deadline starts (see `tests/native/cli.rs`).
    let _ = transcript::ferros_bin();
    harness::run(|t: &TestContext| {
        let fixture = fixture!("nested_calls");
        let in_c = fixture.marker_line("in_c");
        let call_b = fixture.marker_line("call_b");
        let after_calls = fixture.marker_line("after_calls");
        let break_in_c = format!("break nested_calls.rs:{in_c}");
        let break_after = format!("break nested_calls.rs:{after_calls}");
        let commands = vec![
            break_in_c.as_str(),
            break_after.as_str(),
            "continue",
            "up",
            "up",
            "frame",
            "backtrace",
            "down 3",
            "frame",
            "frame 2",
            "thread 0",
            "frame",
            "up",
            "up",
            "next",
            "frame",
            "continue",
            "frame",
            "help",
        ];
        let tag = t.tag();
        let run = transcript::launch_batch(&tag, &fixture, &[], &commands);
        assert_eq!(run.status.code(), Some(1), "{}", transcript_of(&run));
        assert_eq!(run.stderr, "error: already at the newest frame\n", "{}", transcript_of(&run));

        let replies = command_replies(&run.stdout);
        let grandparent = frame_line(&replies, "frame", 0);
        assert!(grandparent.starts_with("#2 "), "{grandparent}");
        assert!(grandparent.contains("::level_a "), "{grandparent}");
        assert!(grandparent.contains(&format!("nested_calls.rs:{call_b}")), "{grandparent}");
        assert!(grandparent.contains(" * "), "{grandparent}");
        assert_eq!(
            marked_lines(command_reply(&replies, "backtrace", 0)),
            vec![grandparent.clone()],
            "{}",
            transcript_of(&run)
        );
        assert_eq!(frame_line(&replies, "frame", 1), grandparent, "down 3 changed the selection");
        assert_eq!(frame_line(&replies, "frame 2", 0), grandparent);

        let at_thread = frame_line(&replies, "frame", 2);
        assert!(at_thread.starts_with("#0 "), "{at_thread}");
        assert!(at_thread.contains("::level_c "), "{at_thread}");
        assert!(at_thread.contains(&format!("nested_calls.rs:{in_c}")), "{at_thread}");

        let after_next = frame_line(&replies, "frame", 3);
        assert!(after_next.starts_with("#0 "), "{after_next}");
        assert!(!after_next.contains("::level_a "), "{after_next}");

        let after_continue = frame_line(&replies, "frame", 4);
        assert!(after_continue.starts_with("#0 "), "{after_continue}");
        assert!(after_continue.contains("::main "), "{after_continue}");
        assert!(
            after_continue.contains(&format!("nested_calls.rs:{after_calls}")),
            "{after_continue}"
        );

        let help = command_reply(&replies, "help", 0);
        for name in ["frame", "up", "down"] {
            assert!(
                help.lines().any(|line| line.split_whitespace().next() == Some(name)),
                "help missing {name}:\n{help}"
            );
        }
    });
}

fn transcript_of(run: &Transcript) -> String
{
    // Built in pieces: one `format!` with both strings trips `large_stack_arrays`.
    let mut text = format!("status: {:?}", run.status);
    text.push_str("\nstdout:\n");
    text.push_str(&run.stdout);
    text.push_str("\nstderr:\n");
    text.push_str(&run.stderr);
    text
}

fn command_replies(stdout: &str) -> Vec<(&str, &str)>
{
    stdout
        .split("(ferros) ")
        .skip(1)
        .map(|chunk| chunk.split_once('\n').unwrap_or((chunk, "")))
        .collect()
}

fn command_reply<'a>(replies: &[(&'a str, &'a str)], command: &str, nth: usize) -> &'a str
{
    replies
        .iter()
        .filter(|(name, _)| *name == command)
        .nth(nth)
        .unwrap_or_else(|| panic!("no reply {nth} to {command:?} in {replies:?}"))
        .1
}

fn frame_line(replies: &[(&str, &str)], command: &str, nth: usize) -> String
{
    let output = command_reply(replies, command, nth);
    output
        .lines()
        .next()
        .unwrap_or_else(|| panic!("empty reply to {command}: {output:?}"))
        .to_owned()
}

fn marked_lines(output: &str) -> Vec<String>
{
    output
        .lines()
        .filter(|line| line.contains(" * "))
        .map(str::to_owned)
        .collect()
}
