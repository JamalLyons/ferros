//! Backtraces through `Session::stack_trace` and the `backtrace` REPL command on real
//! fixture processes (issue #38).
//!
//! Every trace is checked for frames outside executable memory, so a guessed frame (from a
//! stale link register or a stack scan) fails whichever test produced it.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ferros_core::{Address, BreakpointRequest, FrameKind, Session, StackFrame, StopEvent};
use ferros_tests::harness::{self, TestContext};
use ferros_tests::symbols::{function_address, marker_address};
use ferros_tests::transcript::{self, Interactive};
use ferros_tests::{Fixture, fixture};

/// How long a test waits for one stop after resuming.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Frames requested per trace: more than any fixture stack, so a trace that does not end
/// at the thread base shows up as garbage frames rather than a cut-off.
const MAX_FRAMES: usize = 256;

/// Resume until a breakpoint at `address` stops the process.
fn run_to(session: &mut Session, address: Address)
{
    session
        .add_breakpoint(BreakpointRequest::Software { address })
        .unwrap_or_else(|err| panic!("add_breakpoint at {address}: {err}"));
    session.resume().expect("resume");
    match session.wait(Some(STOP_TIMEOUT)).expect("wait") {
        Some(StopEvent::Breakpoint { pc, thread }) if pc == address => {
            session.select_thread(thread).expect("select the stopped thread");
        }
        other => panic!("expected a breakpoint stop at {address}, got {other:?}"),
    }
}

/// `name` without the fixture crate prefix, or `None` for a frame outside the fixture.
fn fixture_function<'a>(frame: &'a StackFrame, fixture: &Fixture) -> Option<&'a str>
{
    let crate_name = fixture.path().file_name()?.to_str()?.replace('-', "_");
    let name = frame.symbol.as_ref()?.display_name();
    name.strip_prefix(&crate_name)?.strip_prefix("::")
}

/// The `(function, line)` of each frame, with the function relative to the fixture
/// crate (`?` outside it) and the line only for frames in the fixture's source file.
fn summary(frames: &[StackFrame], fixture: &Fixture) -> Vec<(String, Option<u32>)>
{
    let source = format!("{}.rs", fixture.name());
    frames
        .iter()
        .map(|frame| {
            let function = fixture_function(frame, fixture).unwrap_or("?");
            let line = frame
                .location
                .as_ref()
                .filter(|location| location.file.ends_with(&source))
                .and_then(|location| location.line);
            (function.to_owned(), line)
        })
        .collect()
}

/// Every frame pc lies in an executable region, indices count up from zero, and the trace
/// ended before `MAX_FRAMES`.
fn assert_sane(session: &mut Session, frames: &[StackFrame])
{
    let regions = session.memory_regions().expect("memory regions");
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame.index, index, "{frames:#?}");
        assert!(
            regions
                .iter()
                .any(|region| region.permissions.contains('x') && region.start <= frame.pc && frame.pc < region.end),
            "frame {index} pc {} is not in an executable region: {frames:#?}",
            frame.pc
        );
    }
    assert!(!frames.is_empty());
    assert!(
        frames.len() < MAX_FRAMES,
        "the trace did not terminate: {} frames",
        frames.len()
    );
}

/// The trace has exactly `expected` (function, line) pairs first, then only frames outside
/// the fixture crate except Rust's runtime start (`std::rt::lang_start` and the closures it
/// calls `main` through, which are generic over the fixture's `main` and so carry its
/// crate in their name).
fn assert_starts_with(frames: &[StackFrame], fixture: &Fixture, expected: &[(&str, u32)])
{
    let summary = summary(frames, fixture);
    let head: Vec<(&str, Option<u32>)> = summary
        .iter()
        .take(expected.len())
        .map(|(function, line)| (function.as_str(), *line))
        .collect();
    let wanted: Vec<(&str, Option<u32>)> = expected.iter().map(|&(function, line)| (function, Some(line))).collect();
    assert_eq!(head, wanted, "{summary:#?}");
    for frame in &frames[expected.len()..] {
        let name = frame.symbol.as_ref().map(|symbol| symbol.display_name().to_owned());
        assert!(
            fixture_function(frame, fixture).is_none() || name.as_deref().is_some_and(|name| name.contains("lang_start")),
            "unexpected fixture frame after main: {summary:#?}"
        );
    }
}

fn nested_expected(fixture: &Fixture, in_c: u32) -> Vec<(&'static str, u32)>
{
    vec![
        ("level_c", in_c),
        ("level_b", fixture.marker_line("call_c")),
        ("level_a", fixture.marker_line("call_b")),
        ("main", fixture.marker_line("call_a")),
    ]
}

/// Stopped in `level_c` of `nested_calls`, the frames are `level_c`, `level_b`, `level_a`,
/// `main` (then only runtime start frames), each at the stop line or its call site.
#[test]
fn backtrace_in_level_c_lists_the_callers_at_their_call_sites()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let in_c = marker_address(&mut d, &fixture, "in_c");
        run_to(&mut d, in_c);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        assert_starts_with(&frames, &fixture, &nested_expected(&fixture, fixture.marker_line("in_c")));
        // The fixture's functions are `#[inline(never)]`: four physical frames, each
        // returning into the next. (Runtime frames below `main` may include std functions
        // inlined into `lang_start`.)
        assert!(frames[..4].iter().all(|frame| !frame.is_inlined()), "{frames:#?}");
        assert_eq!(frames[0].pc, in_c);
        for pair in frames[..5].windows(2) {
            assert_eq!(pair[0].return_address, Some(pair[1].pc), "{frames:#?}");
        }
    });
}

/// Stopped on the first instruction of `level_c`, before its prologue saved anything, the
/// caller is still `level_b` at its call site (the return address is in the link
/// register), and the rest of the trace is unchanged.
#[test]
fn backtrace_at_the_first_instruction_of_level_c_still_finds_level_b()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let entry = function_address(&mut d, &fixture, "level_c");
        run_to(&mut d, entry);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        // The entry instruction belongs to the function's opening line.
        let entry_line = summary(&frames, &fixture)[0].1.expect("a line for level_c's entry");
        assert_starts_with(&frames, &fixture, &nested_expected(&fixture, entry_line));
        assert_eq!(frames[0].pc, entry);
        assert_eq!(frames[0].sp, frames[1].sp, "nothing was pushed before the prologue");
    });
}

/// A stop in `worker_body` on a spawned thread of `threads` gives `worker_body`, the spawn
/// closure, std's thread-start path down to `Thread::new::thread_start` (the function
/// `pthread_create` runs), and then only the unsymbolicated `libsystem_pthread` frames that
/// call it, after which the trace ends.
#[test]
fn backtrace_on_a_spawned_thread_reaches_the_thread_base()
{
    /// `_pthread_start` and `thread_start` in `libsystem_pthread`.
    const PTHREAD_FRAMES: usize = 2;
    harness::run(|t| {
        let fixture = fixture!("threads");
        let mut d = t.launch(&fixture);
        // Suspended at entry, before `main` spawned anything: the only thread is the main
        // thread.
        let threads = d.threads().expect("threads");
        let [main] = threads.as_slice() else {
            panic!("expected one thread at entry, got {threads:?}");
        };
        let main = *main;
        let worker_body = marker_address(&mut d, &fixture, "worker_body");
        run_to(&mut d, worker_body);
        assert_ne!(d.selected_thread(), Some(main), "the stop is on a spawned thread");

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let summary = summary(&frames, &fixture);
        assert_eq!(
            summary[0],
            ("worker_body".to_owned(), Some(fixture.marker_line("worker_body"))),
            "{summary:#?}"
        );
        assert!(
            summary[1].0.starts_with("main::{closure"),
            "the caller is the spawn closure: {summary:#?}"
        );
        assert!(summary.iter().all(|(function, _)| function != "main"), "{summary:#?}");

        let names: Vec<String> = frames
            .iter()
            .map(|frame| {
                frame
                    .symbol
                    .as_ref()
                    .map_or_else(|| "??".to_owned(), |s| s.display_name().to_owned())
            })
            .collect();
        let spawn = names
            .iter()
            .position(|name| name.starts_with("std::thread::lifecycle::spawn_unchecked::"))
            .unwrap_or_else(|| panic!("no std spawn frame: {names:#?}"));
        let base = names
            .iter()
            .position(|name| name.ends_with("::thread_start"))
            .unwrap_or_else(|| panic!("no std thread_start frame: {names:#?}"));
        assert!(1 < spawn && spawn < base, "{names:#?}");
        let below = &frames[base + 1..];
        assert_eq!(below.len(), PTHREAD_FRAMES, "{names:#?}");
        assert!(
            below.iter().all(|frame| frame.symbol.is_none() && frame.pc.value() != 0),
            "{names:#?}"
        );
    });
}

/// Stopped inside `always_inlined` (`#[inline(always)]`) in `symbols`, the inlined function
/// is its own frame at its own line, directly before the physical `main` frame at the line
/// of the call it was inlined into, both at the same pc. (`core::hint::black_box`, inlined
/// into `always_inlined` in turn, may come first.)
#[test]
fn inlined_function_is_its_own_frame_before_the_physical_frame()
{
    harness::run(|t| {
        let fixture = fixture!("symbols");
        let mut d = t.launch(&fixture);
        let in_inlined = marker_address(&mut d, &fixture, "in_inlined");
        run_to(&mut d, in_inlined);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let summary = summary(&frames, &fixture);
        let at = summary
            .iter()
            .position(|(function, _)| function == "always_inlined")
            .unwrap_or_else(|| panic!("no always_inlined frame: {summary:#?}"));
        assert!(at <= 1, "{summary:#?}");
        assert!(frames[..=at].iter().all(StackFrame::is_inlined), "{frames:#?}");
        let (inlined, physical) = (&frames[at], &frames[at + 1]);
        assert_eq!(summary[at].1, Some(fixture.marker_line("in_inlined")), "{summary:#?}");
        let FrameKind::Inlined { physical: owner, .. } = inlined.kind else {
            panic!("{frames:#?}");
        };
        assert_eq!(owner, physical.id);
        assert!(!physical.is_inlined(), "{frames:#?}");
        assert_eq!(physical.pc, inlined.pc);
        let summary = &summary[at..];
        let calls = [
            fixture.marker_line("call_inlined_first"),
            fixture.marker_line("call_inlined_second"),
        ];
        assert_eq!(summary[1].0, "main", "{summary:#?}");
        assert!(
            summary[1].1.is_some_and(|line| calls.contains(&line)),
            "main is at one of the calls {calls:?}: {summary:#?}"
        );
    });
}

/// Stopped inside `leaf_mix`, a frameless leaf (compact unwind `MODE_FRAMELESS`, no
/// `__eh_frame` FDE, `fp` still `main`'s), the caller is `main` at the call, not `main`'s
/// own caller as the frame-pointer chain would give.
#[test]
fn backtrace_in_a_frameless_leaf_keeps_its_caller()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        // The second instruction: past the entry, so the first-instruction rule does not
        // apply, and still on the marked line (the body is one line of loads and an `eor`).
        let in_leaf = function_address(&mut d, &fixture, "leaf_mix") + 4;
        run_to(&mut d, in_leaf);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        assert_starts_with(
            &frames,
            &fixture,
            &[
                ("leaf_mix", fixture.marker_line("in_leaf")),
                ("main", fixture.marker_line("call_leaf")),
            ],
        );
        assert_eq!(frames[0].sp, frames[1].sp, "the leaf allocated no stack");
    });
}

/// Stopped on the first instruction of `exit_success`, a frame-pointer function
/// (compact unwind `MODE_FRAME`, no FDE) that the linker merged into the compact entry of
/// `exit_failure` before it: its frame record is not built yet, so the caller is `main`
/// at the call, not `main`'s own caller as the frame-pointer chain would give.
#[test]
fn backtrace_at_the_entry_of_a_merged_frame_function_keeps_its_caller()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let entry = function_address(&mut d, &fixture, "exit_success");
        run_to(&mut d, entry);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        assert_eq!(frames[0].pc, entry, "{frames:#?}");
        assert_eq!(
            summary(&frames, &fixture)[1],
            ("main".to_owned(), Some(fixture.marker_line("call_exit"))),
            "{frames:#?}"
        );
        assert_eq!(frames[0].sp, frames[1].sp, "nothing was pushed yet");
    });
}

/// A trace with more than 50 frames through `recursion` completes in under a second
/// (symbols loading included), with one `descend` frame per level.
#[test]
fn deep_backtrace_is_fast_and_complete()
{
    const DEPTH: usize = 60;
    harness::run(|t| {
        let fixture = fixture!("recursion");
        let mut d = t.launch_with_args(&fixture, &[&DEPTH.to_string()]);
        let bottom = marker_address(&mut d, &fixture, "bottom");
        run_to(&mut d, bottom);

        let started = Instant::now();
        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        let elapsed = started.elapsed();
        assert!(frames.len() > 50, "{} frames", frames.len());
        assert!(elapsed < Duration::from_secs(1), "{} frames took {elapsed:?}", frames.len());

        assert_sane(&mut d, &frames);
        let summary = summary(&frames, &fixture);
        let recurse = fixture.marker_line("recurse");
        assert_eq!(summary[0], ("descend".to_owned(), Some(fixture.marker_line("bottom"))));
        for (index, frame) in summary[1..=DEPTH].iter().enumerate() {
            assert_eq!(
                frame,
                &("descend".to_owned(), Some(recurse)),
                "frame {}: {summary:#?}",
                index + 1
            );
        }
        assert_eq!(summary[DEPTH + 1].0, "main", "{summary:#?}");
    });
}

/// `bt` in the REPL, stopped in `level_c`, prints `#<index> <pc> in <function> at
/// <file>:<line>` for `level_c`, `level_b`, `level_a` and `main` in order.
#[test]
fn bt_prints_the_frames()
{
    // Build and sign `ferros` before the harness deadline starts (see `tests/cli.rs`).
    let _ = transcript::ferros_bin();
    harness::run(|t: &TestContext| {
        let fixture = fixture!("nested_calls");
        let offset = {
            let mut d = t.launch(&fixture);
            let base = d
                .images()
                .expect("images")
                .into_iter()
                .find(|image| image.path == fixture.path())
                .expect("main image")
                .load_address;
            marker_address(&mut d, &fixture, "in_c").value() - base.value()
        };

        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &fixture, &[]);
        let images = ferros.send("images");
        let path = fixture.path().display().to_string();
        let base = images
            .lines()
            .find(|line| line.ends_with(&path))
            .and_then(|line| line.split(' ').next())
            .and_then(|hex| hex.strip_prefix("0x"))
            .map_or_else(
                || panic!("no image line for {path}: {images}"),
                |hex| u64::from_str_radix(hex, 16).expect("hex"),
            );
        let reply = ferros.send(&format!("break *{:#x}", base + offset));
        assert!(reply.starts_with("breakpoint 1 at "), "{reply:?}");
        let reply = ferros.send("continue");
        assert!(reply.starts_with("stopped: breakpoint 1 at "), "{reply:?}");

        let reply = ferros.send("bt");
        let lines: Vec<&str> = reply.lines().collect();
        let source = format!("{}.rs", fixture.name());
        let crate_name = "ferros_fixture_nested_calls";
        let expected = nested_expected(&fixture, fixture.marker_line("in_c"));
        assert!(lines.len() > expected.len(), "{reply}");
        for (index, ((function, line), text)) in expected.iter().zip(&lines).enumerate() {
            assert!(text.starts_with(&format!("#{index} 0x")), "{text:?} in\n{reply}");
            let tail = format!(" in {crate_name}::{function} at ");
            assert!(text.contains(&tail), "{text:?} lacks {tail:?} in\n{reply}");
            assert!(text.ends_with(&format!("{source}:{line}")), "{text:?} in\n{reply}");
        }
        assert_eq!(ferros.send("backtrace 2").lines().count(), 2);
        let (status, _) = ferros.finish();
        assert!(status.success(), "{status}");
    });
}
