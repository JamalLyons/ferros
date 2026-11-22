//! Backtraces through `Session::stack_trace` and the `backtrace` REPL command on real
//! fixture processes (issue #38).
//!
//! Every trace is checked for frames outside executable memory, so a guessed frame (from a
//! stale link register or a stack scan) fails whichever test produced it.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ferros_core::{
    Address, BreakpointLocation, BreakpointRequest, DebuggerError, FrameKind, Session, StackFrame, StopEvent, SymbolSource,
};
use ferros_tests::harness::{self, TestContext};
use ferros_tests::symbols::{function_address, marker_address, raw_symbol_address};
use ferros_tests::transcript::{self, Interactive};
use ferros_tests::{Fixture, find_marker, fixture};

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

/// `frame` is named from a symbol table only: a name, marked so, and no source location.
fn assert_symbol_table_frame(frame: &StackFrame)
{
    let symbol = frame
        .symbol
        .as_ref()
        .unwrap_or_else(|| panic!("frame {} has no name: {frame:#?}", frame.index));
    assert_eq!(symbol.source(), SymbolSource::SymbolTable, "{frame:#?}");
    assert!(frame.location.is_none(), "{frame:#?}");
    assert!(!frame.is_inlined(), "{frame:#?}");
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

/// Below `main` in `nested_calls`, the runtime frames without DWARF are named from their
/// image's symbol table: a demangled name, marked as a symbol-table name, with no source
/// location. The frames named from DWARF are marked as such and keep their locations
/// (issue #104).
///
/// Whether std's frames (`std::rt::lang_start_internal` and the shims around it) carry
/// DWARF depends on the toolchain and machine, so they may be named either way (issue
/// #135). dyld's `start` never has DWARF from any Rust toolchain, so it is the frame the
/// symbol-table naming is checked on for certain. Any frame that does come from a symbol
/// table under a mangled name must show it demangled.
#[test]
fn frames_without_dwarf_below_main_are_named_from_the_symbol_table()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let in_c = marker_address(&mut d, &fixture, "in_c");
        run_to(&mut d, in_c);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let names: Vec<String> = frames
            .iter()
            .map(|frame| {
                frame
                    .symbol
                    .as_ref()
                    .map_or_else(|| "??".to_owned(), |s| s.display_name().to_owned())
            })
            .collect();
        assert!(names.iter().all(|name| name != "??"), "every frame is named: {names:#?}");
        for frame in &frames[..4] {
            let symbol = frame.symbol.as_ref().expect("a DWARF name");
            assert_eq!(symbol.source(), SymbolSource::DebugInfo, "{frame:#?}");
            assert!(frame.location.is_some(), "{frame:#?}");
        }
        for frame in &frames[4..] {
            let symbol = frame.symbol.as_ref().expect("a name");
            match symbol.source() {
                SymbolSource::DebugInfo => assert!(frame.location.is_some(), "{frame:#?}"),
                SymbolSource::SymbolTable => {
                    assert_symbol_table_frame(frame);
                    // Demangled: `std::rt::lang_start_internal`, not `__ZN3std2rt19...`.
                    let name = symbol.display_name();
                    assert!(!name.starts_with("_ZN") && !name.starts_with("__ZN"), "{names:#?}");
                    if symbol.raw().trim_start_matches('_').starts_with("ZN") {
                        assert_ne!(name, symbol.raw(), "a mangled name is shown demangled: {frame:#?}");
                    }
                }
            }
        }
        // dyld's `start` is the base, and no Rust toolchain ships DWARF for it: it is
        // certain to be named from the symbol table, demangled (no `_ZN` prefix).
        let base = frames.last().expect("a base frame");
        assert_symbol_table_frame(base);
        assert_eq!(
            names.last().map(String::as_str),
            Some("start"),
            "dyld's start is the base: {names:#?}"
        );
    });
}

/// dyld leaves itself out of the image array it publishes; the backend lists it from
/// `dyldImageLoadAddress`, after the main executable. The main-thread base frame, dyld's
/// `start`, then lies in a listed image and is named from its symbol table without the
/// header search for unlisted code (issue #121).
#[test]
fn dyld_is_a_listed_image_and_names_the_main_thread_base()
{
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const MH_DYLINKER: u32 = 7;
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let in_c = marker_address(&mut d, &fixture, "in_c");
        run_to(&mut d, in_c);

        let images = d.images().expect("images");
        assert_eq!(
            images.first().map(|image| image.path.as_path()),
            Some(fixture.path()),
            "the main executable is first: {images:#?}"
        );
        let dylds: Vec<_> = images
            .iter()
            .filter(|image| image.path.file_name().is_some_and(|name| name == "dyld"))
            .collect();
        assert_eq!(dylds.len(), 1, "dyld is listed once: {images:#?}");
        let dyld = dylds[0];
        assert!(dyld.path.is_absolute(), "{dyld:?}");
        let header = d.read_memory(dyld.load_address, 16).expect("read dyld's header");
        assert_eq!(header[..4], MH_MAGIC_64.to_le_bytes(), "{dyld:?}");
        assert_eq!(header[12..16], MH_DYLINKER.to_le_bytes(), "dyld's filetype: {dyld:?}");

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let base = frames.last().expect("a base frame");
        assert_symbol_table_frame(base);
        assert_eq!(
            base.symbol.as_ref().map(ferros_core::SymbolName::display_name),
            Some("start"),
            "{frames:#?}"
        );
        // `start` lies in dyld: the listed image that contains the base pc is dyld, so
        // the name comes from a listed image rather than an image found by search.
        let owner = images
            .iter()
            .filter(|image| image.load_address <= base.pc)
            .max_by_key(|image| image.load_address)
            .expect("an image below the base pc");
        assert_eq!(owner.load_address, dyld.load_address, "base {base:#?} in {images:#?}");
    });
}

/// At the launch stop dyld has run no instruction and records nothing, yet it is listed:
/// the `MH_DYLINKER` header the kernel mapped, after the main executable. The stopped pc
/// (dyld's entry) lies in its code and is named from its symbol table (issue #121).
#[test]
fn dyld_is_listed_at_the_launch_stop()
{
    const MH_DYLINKER: u32 = 7;
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let images = d.images().expect("images at the launch stop");
        assert_eq!(
            images.first().map(|image| image.path.as_path()),
            Some(fixture.path()),
            "{images:#?}"
        );
        let dylds: Vec<_> = images
            .iter()
            .filter(|image| image.path.file_name().is_some_and(|name| name == "dyld"))
            .collect();
        assert_eq!(dylds.len(), 1, "dyld is listed once: {images:#?}");
        let header = d.read_memory(dylds[0].load_address, 16).expect("read dyld's header");
        assert_eq!(header[12..16], MH_DYLINKER.to_le_bytes(), "{images:#?}");

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let entry = &frames[0];
        assert!(dylds[0].load_address < entry.pc, "{entry:#?} in {images:#?}");
        assert!(
            images
                .iter()
                .all(|image| image.load_address <= dylds[0].load_address || image.load_address > entry.pc),
            "the image containing the entry is dyld: {entry:#?} in {images:#?}"
        );
        assert_symbol_table_frame(entry);
        assert_eq!(
            entry.symbol.as_ref().map(ferros_core::SymbolName::display_name),
            Some("_dyld_start"),
            "{entry:#?}"
        );
    });
}

/// At the launch stop dyld runs from its file, `/usr/lib/dyld`, which is universal: one
/// slice per architecture. Its symbols are read from the slice the kernel mapped, so
/// symbolicating dyld's entry finds a name or reports that slice's missing debug info;
/// it never fails to parse the universal file (issue #169).
#[test]
fn dyld_entry_symbolicates_from_its_universal_file_at_the_launch_stop()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let images = d.images().expect("images at the launch stop");
        let dyld = images
            .iter()
            .find(|image| image.path.file_name().is_some_and(|name| name == "dyld"))
            .unwrap_or_else(|| panic!("dyld is listed: {images:#?}"))
            .clone();
        let entry = d.stack_trace(1).expect("stack trace")[0].pc;
        assert!(dyld.load_address < entry, "{entry} in {images:#?}");
        for attempt in 0..2 {
            match d.symbolicate(entry) {
                Ok(_) => {}
                Err(DebuggerError::NoDebugInfo { path, detail }) => {
                    assert_eq!(path, dyld.path, "attempt {attempt}: {detail}");
                }
                Err(error) => panic!("attempt {attempt}: symbolicating dyld's entry {entry}: {error}"),
            }
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
/// `pthread_create` runs), and then the two `libsystem_pthread` frames that call it,
/// `_pthread_start` and `thread_start`, after which the trace ends. `libsystem_pthread`
/// lives in the dyld shared cache and has no DWARF: its names come from the symbol table
/// in the cache's `__LINKEDIT`, so they carry no source location.
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
        assert_eq!(&names[base + 1..], ["_pthread_start", "thread_start"], "{names:#?}");
        for frame in below {
            assert_symbol_table_frame(frame);
            assert_ne!(frame.pc.value(), 0, "{names:#?}");
        }
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

/// Stopped in `on_sigusr1`, the SIGUSR1 handler of `signal_handler --interrupt`, after the
/// signal interrupted `spin` on `spin_line`: the trace is the handler at its line, one
/// frame for `_sigtramp` (outside the fixture, named or not), then `spin` at the
/// interrupted line (looked up at its pc, not as a return address), then `main` at the
/// call. Following `_sigtramp`'s frame record instead skips `spin`.
#[test]
fn backtrace_in_a_signal_handler_shows_the_interrupted_function()
{
    /// `SIGUSR1` on macOS; the test harness uses no raw FFI.
    const SIGUSR1: i32 = 30;
    harness::run(|t| {
        let fixture = fixture!("signal_handler");
        let mut d = t.launch_with_args(&fixture, &["--interrupt"]);
        let pid = d.pid().0;
        let spin_line = marker_address(&mut d, &fixture, "spin_line");
        run_to(&mut d, spin_line);
        let ids: Vec<_> = d.breakpoints().iter().map(|bp| bp.id).collect();
        for id in ids {
            d.remove_breakpoint(id).expect("remove the spin breakpoint");
        }
        let in_handler = marker_address(&mut d, &fixture, "in_handler");
        d.add_breakpoint(BreakpointRequest::Software { address: in_handler })
            .expect("break in the handler");

        // Sent while stopped; the spinning thread takes it once resumed.
        let status = std::process::Command::new("/bin/kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .expect("run /bin/kill");
        assert!(status.success());
        d.resume().expect("resume into the signal");
        match d.wait(Some(STOP_TIMEOUT)).expect("wait for the signal") {
            Some(StopEvent::Signal { signal: SIGUSR1, .. }) => {}
            other => panic!("expected the SIGUSR1 stop, got {other:?}"),
        }
        let interrupted = d.read_registers().expect("registers at the signal").pc;
        d.resume_with_signal().expect("deliver SIGUSR1");
        match d.wait(Some(STOP_TIMEOUT)).expect("wait for the handler") {
            Some(StopEvent::Breakpoint { pc, thread }) if pc == in_handler => {
                d.select_thread(thread).expect("select the handler thread");
            }
            other => panic!("expected a stop in the handler, got {other:?}"),
        }

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let summary = summary(&frames, &fixture);
        assert_eq!(
            summary[0],
            ("on_sigusr1".to_owned(), Some(fixture.marker_line("in_handler"))),
            "{summary:#?}"
        );
        assert!(
            fixture_function(&frames[1], &fixture).is_none(),
            "frame 1 is _sigtramp, outside the fixture: {summary:#?}"
        );
        assert_starts_with(
            &frames[2..],
            &fixture,
            &[
                ("spin", fixture.marker_line("spin_line")),
                ("main", fixture.marker_line("call_spin")),
            ],
        );
        // The interrupted frame's pc is the instruction the signal interrupted, a pc in
        // the spin loop, not a return address.
        assert_eq!(frames[2].pc, interrupted, "{frames:#?}");
        assert_eq!(frames[1].return_address, Some(frames[2].pc), "{frames:#?}");
        d.kill().expect("kill");
    });
}

/// Like [`backtrace_in_a_signal_handler_shows_the_interrupted_function`], with the handler
/// on an alternate signal stack (`signal_handler --alt-stack`, issue #129) that lies above
/// the interrupted `spin`: the step from `_sigtramp` to `spin` moves `sp` down. The trace
/// is the handler, one frame for `_sigtramp`, `spin` at the interrupted line, its caller
/// at the call, then `main` at the call into the helper that installed the handler.
/// Rejecting the downward step falls back to `_sigtramp`'s frame record and skips `spin`.
#[test]
fn backtrace_in_a_signal_handler_on_an_alternate_stack_shows_the_interrupted_function()
{
    /// `SIGUSR1` on macOS; the test harness uses no raw FFI.
    const SIGUSR1: i32 = 30;
    harness::run(|t| {
        let fixture = fixture!("signal_handler");
        let mut d = t.launch_with_args(&fixture, &["--alt-stack"]);
        let pid = d.pid().0;
        let spin_line = marker_address(&mut d, &fixture, "spin_line");
        run_to(&mut d, spin_line);
        let ids: Vec<_> = d.breakpoints().iter().map(|bp| bp.id).collect();
        for id in ids {
            d.remove_breakpoint(id).expect("remove the spin breakpoint");
        }
        let in_handler = marker_address(&mut d, &fixture, "in_handler");
        d.add_breakpoint(BreakpointRequest::Software { address: in_handler })
            .expect("break in the handler");

        // Sent while stopped; the spinning thread takes it once resumed.
        let status = std::process::Command::new("/bin/kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .expect("run /bin/kill");
        assert!(status.success());
        d.resume().expect("resume into the signal");
        match d.wait(Some(STOP_TIMEOUT)).expect("wait for the signal") {
            Some(StopEvent::Signal { signal: SIGUSR1, .. }) => {}
            other => panic!("expected the SIGUSR1 stop, got {other:?}"),
        }
        let interrupted = d.read_registers().expect("registers at the signal");
        d.resume_with_signal().expect("deliver SIGUSR1");
        match d.wait(Some(STOP_TIMEOUT)).expect("wait for the handler") {
            Some(StopEvent::Breakpoint { pc, thread }) if pc == in_handler => {
                d.select_thread(thread).expect("select the handler thread");
            }
            other => panic!("expected a stop in the handler, got {other:?}"),
        }
        // The handler runs on the alternate stack, above the interrupted frame.
        let handler_sp = d.read_registers().expect("registers in the handler").sp;
        assert!(
            handler_sp > interrupted.sp,
            "handler sp {handler_sp} interrupted sp {}",
            interrupted.sp
        );

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        let summary = summary(&frames, &fixture);
        assert!(summary.len() > 4, "{summary:#?}");
        assert_eq!(
            summary[0],
            ("on_sigusr1".to_owned(), Some(fixture.marker_line("in_handler"))),
            "{summary:#?}"
        );
        assert!(
            fixture_function(&frames[1], &fixture).is_none(),
            "frame 1 is _sigtramp, outside the fixture: {summary:#?}"
        );
        assert_eq!(
            summary[2..4],
            vec![
                ("spin".to_owned(), Some(fixture.marker_line("spin_line"))),
                (
                    "spin_below_alternate_stack".to_owned(),
                    Some(fixture.marker_line("call_spin_alt"))
                ),
            ],
            "{summary:#?}"
        );
        assert!(
            summary[4..].contains(&("main".to_owned(), Some(fixture.marker_line("call_alt")))),
            "{summary:#?}"
        );
        assert_eq!(frames[2].pc, interrupted.pc, "{frames:#?}");
        assert_eq!(frames[2].sp, interrupted.sp, "{frames:#?}");
        assert!(frames[2].sp < frames[1].sp, "the signal step moved sp down: {frames:#?}");
        assert_eq!(frames[1].return_address, Some(frames[2].pc), "{frames:#?}");
        d.kill().expect("kill");
    });
}

/// The instruction word at `address`, as the program sees it (traps hidden).
fn instruction(session: &mut Session, address: Address) -> u32
{
    let bytes = session.read_memory(address, 4).expect("read code");
    u32::from_le_bytes(bytes.try_into().expect("4 bytes"))
}

/// Address of the first instruction at or after `start` that `matches`, within 64
/// instructions.
fn find_instruction(session: &mut Session, start: Address, matches: impl Fn(u32) -> bool) -> Address
{
    (0..64)
        .map(|index| Address::new(start.value() + 4 * index))
        .find(|&address| matches(instruction(session, address)))
        .unwrap_or_else(|| panic!("no matching instruction within 64 after {start}"))
}

/// `add x29, sp, #imm` (`mov x29, sp` included): the instruction that sets the frame
/// pointer to the new frame record.
fn sets_frame_pointer(insn: u32) -> bool
{
    insn & 0xFF80_03FF == 0x9100_03FD
}

/// Resume until the next breakpoint stop, which must be at `address`.
fn continue_to(session: &mut Session, address: Address)
{
    session.resume().expect("resume");
    match session.wait(Some(STOP_TIMEOUT)).expect("wait") {
        Some(StopEvent::Breakpoint { pc, thread }) if pc == address => {
            session.select_thread(thread).expect("select the stopped thread");
        }
        other => panic!("expected a breakpoint stop at {address}, got {other:?}"),
    }
}

/// Stopped inside the prologue of `exit_success` (compact unwind `MODE_FRAME`, merged into
/// `exit_failure`'s entry), past its first instruction and up to the instruction that
/// sets `x29`, the frame record is not built yet: the caller is `main` at the call, with
/// the `sp` the prologue allocated added back. The frame-pointer chain would follow
/// `main`'s own record and skip `main`. Both stops have a breakpoint on an analysed
/// instruction, so the unwinder must read the original code under the traps.
#[test]
fn backtrace_in_the_prologue_of_a_frame_function_keeps_its_caller()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let entry = function_address(&mut d, &fixture, "exit_success");
        let sets_fp = find_instruction(&mut d, entry, sets_frame_pointer);
        let stops: Vec<Address> = (entry.value() + 4..=sets_fp.value()).step_by(4).map(Address::new).collect();
        assert!(!stops.is_empty(), "exit_success has a prologue after its entry");
        for &stop in &stops {
            d.add_breakpoint(BreakpointRequest::Software { address: stop })
                .unwrap_or_else(|err| panic!("add_breakpoint at {stop}: {err}"));
        }

        for &stop in &stops {
            continue_to(&mut d, stop);
            let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
            assert_sane(&mut d, &frames);
            assert_eq!(frames[0].pc, stop, "{frames:#?}");
            assert_eq!(
                summary(&frames, &fixture)[1],
                ("main".to_owned(), Some(fixture.marker_line("call_exit"))),
                "stopped at {stop}: {frames:#?}"
            );
            assert!(frames[1].sp > frames[0].sp, "the prologue allocated stack: {frames:#?}");
        }
    });
}

/// Stopped on the `ret` of the C `_main` shim (compact unwind `MODE_FRAME`, no FDE) after
/// the fixture's `main` returned: its epilogue already reloaded `x29` and `x30`, so `fp` is
/// dyld's and the frame record is gone. The caller is dyld's `start`, through the link
/// register, with `sp` unchanged; the frame-pointer chain would skip `start`.
#[test]
fn backtrace_after_the_epilogue_of_a_frame_function_keeps_its_caller()
{
    harness::run(|t| {
        let fixture = fixture!("hello");
        let mut d = t.launch(&fixture);
        let print = marker_address(&mut d, &fixture, "hello_print");
        run_to(&mut d, print);
        // `_main` is the frame below Rust's runtime start; its pc is the return address of
        // the call to `lang_start`, a few instructions before its `ret`.
        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        let shim = frames
            .iter()
            .find(|frame| frame.symbol.as_ref().is_some_and(|symbol| symbol.display_name() == "main"))
            .unwrap_or_else(|| panic!("no C `main` frame: {frames:#?}"));
        let ret = find_instruction(&mut d, shim.pc, |insn| insn == 0xD65F_03C0);
        let start = frames[shim.index + 1].pc;
        let breakpoints: Vec<_> = d.breakpoints().into_iter().map(|info| info.id).collect();
        for id in breakpoints {
            d.remove_breakpoint(id).expect("remove breakpoint");
        }
        run_to(&mut d, ret);

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        assert_eq!(frames[0].pc, ret, "{frames:#?}");
        assert!(frames.len() >= 2, "the caller of `_main` is missing: {frames:#?}");
        assert_eq!(frames[1].pc, start, "the caller is dyld's `start` at the call: {frames:#?}");
        assert_eq!(frames[0].sp, frames[1].sp, "the epilogue released the frame");
    });
}

/// `bl <label>`.
fn is_branch_with_link(insn: u32) -> bool
{
    insn & 0xFC00_0000 == 0x9400_0000
}

/// `frameless_spill` of `frameless_stack` is a leaf with a 32-byte stack allocation and no
/// frame record (compact unwind `MODE_FRAMELESS`, stack size 32, no FDE), called from the
/// initializer `frameless_caller`. Stopped on its `add sp` the allocation is still there,
/// so the caller's `sp` is 32 higher. Stopped on its `ret`, after `add sp` released the
/// allocation, the caller's `sp` is the stopped `sp`: the compact encoding alone would
/// add the stack size again (issue #130). Either way the caller is `frameless_caller` at
/// the return address of its call. Both stops have a breakpoint on an analysed
/// instruction, so the unwinder must read the original code under the traps.
#[test]
fn backtrace_after_the_epilogue_of_a_frameless_function_keeps_the_callers_sp()
{
    harness::run(|t| {
        let fixture = fixture!("frameless_stack");
        let mut d = t.launch(&fixture);
        let leaf = raw_symbol_address(&mut d, &fixture, "_frameless_spill");
        let caller = raw_symbol_address(&mut d, &fixture, "_frameless_caller");
        let release = find_instruction(&mut d, leaf, |insn| insn == 0x9100_83FF); // add sp, sp, #0x20
        let ret = find_instruction(&mut d, leaf, |insn| insn == 0xD65F_03C0);
        assert_eq!(ret.value(), release.value() + 4, "the `ret` follows the `add sp`");
        let return_address = find_instruction(&mut d, caller, is_branch_with_link) + 4;
        for stop in [release, ret] {
            d.add_breakpoint(BreakpointRequest::Software { address: stop })
                .unwrap_or_else(|err| panic!("add_breakpoint at {stop}: {err}"));
        }

        for (stop, allocated) in [(release, 0x20), (ret, 0)] {
            continue_to(&mut d, stop);
            let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
            assert_sane(&mut d, &frames);
            assert_eq!(frames[0].pc, stop, "{frames:#?}");
            assert!(frames.len() >= 2, "the caller is missing: {frames:#?}");
            assert_eq!(
                frames[1].pc, return_address,
                "stopped at {stop}, the caller is `frameless_caller` after its call: {frames:#?}"
            );
            assert_eq!(
                frames[1].sp.value(),
                frames[0].sp.value() + allocated,
                "stopped at {stop}: {frames:#?}"
            );
        }
    });
}

/// Stopped in `init_routine`, the frame directly above it is `__pthread_once_handler`.
///
/// That function is static (non-external) in `libsystem_pthread`, a library in the dyld
/// shared cache. Current macOS keeps each cache library's local symbols in the mapped
/// symbol table: there is no separate `.symbols` file, and the cache header's
/// local-symbols range is empty, so the name comes from the image's own `LC_SYMTAB`
/// (issue #121). The Mach-O symbol is `___pthread_once_handler`; one leading underscore
/// is stripped.
///
/// Older caches move local symbols out of the mapped table into a separate `.symbols`
/// file beside the cache, which is not read. There the function cannot be named, and the
/// test only reports why (issue #121 accepts a trace that documents it).
#[test]
fn backtrace_through_a_static_shared_cache_function_names_it()
{
    harness::run(|t| {
        let fixture = fixture!("pthread_once");
        let mut d = t.launch(&fixture);
        let source_path = fixture.source_path().with_file_name("c").join("pthread_once.c");
        let source =
            std::fs::read_to_string(&source_path).unwrap_or_else(|err| panic!("read {}: {err}", source_path.display()));
        let marker_line = |marker: &str| {
            find_marker(&source, marker).unwrap_or_else(|err| panic!("marker {marker} in pthread_once.c: {err:?}"))
        };
        let in_init = marker_line("in_init");
        d.set_breakpoint(BreakpointLocation::Line {
            file: "pthread_once.c".into(),
            line: in_init,
        })
        .expect("break in init_routine");
        d.resume().expect("resume");
        match d.wait(Some(STOP_TIMEOUT)).expect("wait") {
            Some(StopEvent::Breakpoint { thread, .. }) => {
                d.select_thread(thread).expect("select the stopped thread");
            }
            other => panic!("expected a breakpoint stop in init_routine, got {other:?}"),
        }

        let frames = d.stack_trace(MAX_FRAMES).expect("stack trace");
        assert_sane(&mut d, &frames);
        assert!(
            frames.len() > 1,
            "static __pthread_once_handler is absent from the mapped symbol table of libsystem_pthread: no caller of \
             init_routine: {frames:#?}"
        );

        let init = &frames[0];
        let init_symbol = init.symbol.as_ref().expect("init_routine has a DWARF name");
        assert_eq!(init_symbol.display_name(), "init_routine", "{frames:#?}");
        assert_eq!(init_symbol.source(), SymbolSource::DebugInfo, "{frames:#?}");
        let init_at = init.location.as_ref().expect("init_routine has a source location");
        assert!(init_at.file.ends_with("pthread_once.c"), "{init_at:?}");
        assert_eq!(init_at.line, Some(in_init), "{init_at:?}");

        let handler = &frames[1];
        let handler_name = handler.symbol.as_ref().map(ferros_core::SymbolName::display_name);
        let separate = separate_local_symbols();
        if let Some(file) = &separate {
            eprintln!(
                "the dyld shared cache keeps local symbols in {}, which is not read, so __pthread_once_handler cannot be \
                 named (the frame above init_routine is {handler_name:?})",
                file.display()
            );
        } else {
            assert_eq!(
                handler_name,
                Some("__pthread_once_handler"),
                "the frame above init_routine is {handler_name:?}: {frames:#?}"
            );
            assert_symbol_table_frame(handler);
        }

        let images = d.images().expect("images");
        let image = images
            .iter()
            .filter(|image| image.load_address <= handler.pc)
            .max_by_key(|image| image.load_address)
            .unwrap_or_else(|| panic!("no image contains {}: {images:?}", handler.pc));
        assert!(
            image.path.ends_with("libsystem_pthread.dylib"),
            "pc {} of __pthread_once_handler is in {}, not libsystem_pthread.dylib",
            handler.pc,
            image.path.display()
        );

        let call_once = marker_line("call_once");
        let main = frames
            .iter()
            .find(|frame| frame.symbol.as_ref().is_some_and(|symbol| symbol.display_name() == "main"))
            .unwrap_or_else(|| panic!("no main frame: {frames:#?}"));
        assert!(main.index > handler.index, "main is below the handler: {frames:#?}");
        let main_symbol = main.symbol.as_ref().expect("main has a name");
        assert_eq!(main_symbol.source(), SymbolSource::DebugInfo, "{main:#?}");
        let main_at = main.location.as_ref().expect("main has a DWARF location");
        assert!(main_at.file.ends_with("pthread_once.c"), "{main_at:?}");
        assert_eq!(main_at.line, Some(call_once), "{main_at:?}");

        assert!(
            separate.is_some() || frames.iter().all(|frame| frame.symbol.is_some()),
            "an unnamed frame: {frames:#?}"
        );
    });
}

/// Directories that hold the dyld shared cache, newest layout first.
const SHARED_CACHE_DIRS: [&str; 2] = [
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
    "/System/Library/dyld",
];

/// The separate local-symbols file of this system's dyld shared cache, if it has one: a
/// `*.symbols` file beside the cache files.
fn separate_local_symbols() -> Option<std::path::PathBuf>
{
    SHARED_CACHE_DIRS
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "symbols"))
}
