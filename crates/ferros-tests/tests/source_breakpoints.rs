//! Source-level breakpoints, by `file:line` and by function name, against real fixture
//! processes (issue #37).
//!
//! `Session::set_breakpoint` resolves a location through the main executable's debug info
//! into one logical breakpoint with one site per address; a stop reports the logical id
//! and the site's function and source line. The last tests drive the same through the
//! `ferros` binary.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::backend::StdioSpec;
use ferros_core::symbols::SourceLookupError;
use ferros_core::{
    BreakpointHit, BreakpointLocation, DebuggerError, LaunchSpec, LogicalBreakpointId, Session, SessionState, StopEvent,
};
use ferros_macos::MacOSBackend;
use ferros_tests::harness::{self, TestContext};
use ferros_tests::symbols::function_address;
use ferros_tests::transcript::{self, Interactive, normalize};
use ferros_tests::{Fixture, fixture};

/// How long a test waits for one stop after resuming.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The fixture's source file name, matched as a path suffix (`loop_counter.rs`).
fn source_name(fixture: &Fixture) -> String
{
    format!("{}.rs", fixture.name())
}

/// A `file:line` location at `marker` of `fixture`.
fn at_marker(fixture: &Fixture, marker: &str) -> BreakpointLocation
{
    BreakpointLocation::Line {
        file: source_name(fixture),
        line: fixture.marker_line(marker),
    }
}

/// A function-name location.
fn function(name: &str) -> BreakpointLocation
{
    BreakpointLocation::Function(name.to_owned())
}

fn set(session: &mut Session, location: BreakpointLocation) -> LogicalBreakpointId
{
    let shown = location.to_string();
    session
        .set_breakpoint(location)
        .unwrap_or_else(|err| panic!("set_breakpoint at {shown}: {err}"))
}

/// Resume and wait for the next stop, which must be at a logical breakpoint.
fn continue_to_hit(session: &mut Session) -> BreakpointHit
{
    session.resume().expect("resume");
    let event = session
        .wait(Some(STOP_TIMEOUT))
        .expect("wait")
        .unwrap_or_else(|| panic!("no stop within {STOP_TIMEOUT:?}"));
    assert!(
        matches!(event, StopEvent::Breakpoint { .. }),
        "expected a breakpoint stop, got {event:?}"
    );
    session
        .breakpoint_hit()
        .unwrap_or_else(|| panic!("the stop {event:?} belongs to no logical breakpoint"))
}

fn sites(session: &Session, id: LogicalBreakpointId) -> Vec<ferros_core::BreakpointSite>
{
    session
        .logical_breakpoints()
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .expect("the breakpoint")
        .sites
        .clone()
}

/// Whether a lookup error is the expected variant.
type IsExpected = fn(&SourceLookupError) -> bool;

/// The crate path of a fixture function: `ferros_fixture_<name>::<function>`.
fn qualified(fixture: &Fixture, function: &str) -> String
{
    format!("ferros_fixture_{}::{function}", fixture.name())
}

/// `break loop_counter.rs:<tick_body>` stops at that line in `tick` on every iteration,
/// and the hit count follows. AC1 (engine side).
#[test]
fn file_line_breakpoint_hits_on_every_iteration()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &["5"]);
        let line = fixture.marker_line("tick_body");
        let id = set(&mut d, at_marker(&fixture, "tick_body"));
        let [site] = sites(&d, id).try_into().expect("one site");
        assert_eq!(site.position.line, Some(line));
        assert!(
            site.position
                .file
                .as_deref()
                .is_some_and(|file| file.ends_with("/fixtures/loop_counter.rs")),
            "{site:?}"
        );
        assert_eq!(site.position.function.as_deref(), Some(qualified(&fixture, "tick").as_str()));

        for iteration in 1..=3 {
            let hit = continue_to_hit(&mut d);
            assert_eq!(hit.id, id);
            assert_eq!(hit.site, site, "iteration {iteration}");
            assert_eq!(d.logical_breakpoints()[0].hit_count, iteration);
        }
    });
}

/// `break level_c` stops in `level_c` after its prologue, not at its first instruction,
/// at the `in_c` line. AC2.
#[test]
fn function_breakpoint_stops_after_the_prologue()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let entry = function_address(&mut d, &fixture, "level_c");
        let id = set(&mut d, function("level_c"));
        let [site] = sites(&d, id).try_into().expect("one site");
        assert!(site.address > entry, "{site:?} is not after the entry {entry}");

        let hit = continue_to_hit(&mut d);
        assert_eq!(hit.id, id);
        assert_eq!(hit.site.address, site.address);
        assert_eq!(
            hit.site.position.function.as_deref(),
            Some(qualified(&fixture, "level_c").as_str())
        );
        assert_eq!(hit.site.position.line, Some(fixture.marker_line("in_c")));
        assert_eq!(d.read_registers().expect("registers").pc, site.address);
    });
}

/// A generic function with two instantiations gives one breakpoint with two sites, and
/// the program stops at both, each counted on the same breakpoint.
#[test]
fn generic_function_stops_at_both_instantiations()
{
    harness::run(|t| {
        let fixture = fixture!("symbols");
        let mut d = t.launch(&fixture);
        let id = set(&mut d, function("generic_identity"));
        let sites = sites(&d, id);
        assert_eq!(sites.len(), 2, "{sites:?}");

        let mut hit_addresses = vec![continue_to_hit(&mut d).site.address, continue_to_hit(&mut d).site.address];
        hit_addresses.sort_unstable();
        assert_eq!(hit_addresses, sites.iter().map(|site| site.address).collect::<Vec<_>>());
        assert_eq!(d.logical_breakpoints()[0].hit_count, 2);
        assert_eq!(d.logical_breakpoints().len(), 1);

        // Disable and delete act on both sites at once.
        d.disable_logical_breakpoint(id).expect("disable");
        assert!(d.breakpoints().iter().all(|info| !info.enabled));
        d.delete_breakpoint(id).expect("delete");
        assert_eq!(d.breakpoints().len(), 0);
        assert_eq!(d.logical_breakpoints().len(), 0);
    });
}

/// `shapes::area` starts with inlined `wrapping_mul`, whose rows are in a std file; the
/// stop still reports `fixtures/symbols.rs` at the `in_module` line. AC6.
#[test]
fn function_with_inlined_std_reports_its_own_source_line()
{
    harness::run(|t| {
        let fixture = fixture!("symbols");
        let mut d = t.launch(&fixture);
        let id = set(&mut d, function("shapes::area"));
        let hit = continue_to_hit(&mut d);
        assert_eq!(hit.id, id);
        let file = hit.site.position.file.clone().unwrap_or_default();
        assert!(file.ends_with("/fixtures/symbols.rs"), "{:?}", hit.site);
        assert_eq!(hit.site.position.line, Some(fixture.marker_line("in_module")));
        assert_eq!(
            hit.site.position.function.as_deref(),
            Some(qualified(&fixture, "shapes::area").as_str())
        );
    });
}

/// The comment line in `with_gap` has no code: the breakpoint reports the next line with
/// code (`after_gap`), and the stop is there. AC3.
#[test]
fn line_without_code_reports_the_line_used()
{
    harness::run(|t| {
        let fixture = fixture!("symbols");
        let mut d = t.launch(&fixture);
        let comment = fixture.marker_line("before_gap") + 1;
        let after_gap = fixture.marker_line("after_gap");
        assert!(comment < after_gap);
        let id = set(
            &mut d,
            BreakpointLocation::Line {
                file: source_name(&fixture),
                line: comment,
            },
        );
        let [site] = sites(&d, id).try_into().expect("one site");
        assert_eq!(site.position.line, Some(after_gap));
        let hit = continue_to_hit(&mut d);
        assert_eq!(hit.site.position.line, Some(after_gap));
    });
}

/// Unknown and ambiguous files and functions keep their `SourceLookupError` variant, add
/// nothing, and leave the session usable. AC4, AC7.
#[test]
fn lookup_errors_are_typed_and_leave_the_session_usable()
{
    harness::run(|t| {
        let fixture = fixture!("symbols");
        let mut d = t.launch(&fixture);
        let cases: Vec<(BreakpointLocation, IsExpected)> = vec![
            (
                BreakpointLocation::Line {
                    file: "no_such_file.rs".into(),
                    line: 1,
                },
                |error| matches!(error, SourceLookupError::UnknownFile { .. }),
            ),
            (
                BreakpointLocation::Line {
                    file: "twin.rs".into(),
                    line: 1,
                },
                |error| matches!(error, SourceLookupError::AmbiguousFile { .. }),
            ),
            (
                BreakpointLocation::Line {
                    file: source_name(&fixture),
                    line: 100_000,
                },
                |error| matches!(error, SourceLookupError::NoCodeAtOrAfterLine { .. }),
            ),
            (function("no_such_function"), |error| {
                matches!(error, SourceLookupError::UnknownFunction { .. })
            }),
            (function("twin"), |error| {
                matches!(error, SourceLookupError::AmbiguousFunction { .. })
            }),
        ];
        for (location, expected) in cases {
            let shown = location.to_string();
            match d.set_breakpoint(location) {
                Err(DebuggerError::SourceLookup(error)) if expected(&error) => {}
                other => panic!("{shown}: unexpected {other:?}"),
            }
        }
        assert_eq!(d.logical_breakpoints().len(), 0);
        assert_eq!(d.breakpoints().len(), 0);

        // The session still works: a good breakpoint is set and hit.
        let id = set(&mut d, function("Counter::bump"));
        assert_eq!(continue_to_hit(&mut d).id, id);
    });
}

/// The file a breakpoint resolved to is exactly the file its stop reports, and equals the
/// spelling of the line index (`line_addresses`). AC8.
#[test]
fn resolved_file_equals_the_file_reported_at_the_stop()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let by_line = set(&mut d, at_marker(&fixture, "in_b"));
        let by_name = set(&mut d, function("level_a"));
        let line_file = sites(&d, by_line)[0].position.file.clone().expect("file");
        let name_file = sites(&d, by_name)[0].position.file.clone().expect("file");
        assert_eq!(line_file, name_file);

        let first = continue_to_hit(&mut d);
        assert_eq!(first.id, by_name);
        assert_eq!(first.site.position.file.as_deref(), Some(name_file.as_str()));
        let second = continue_to_hit(&mut d);
        assert_eq!(second.id, by_line);
        assert_eq!(second.site.position.file.as_deref(), Some(line_file.as_str()));
        // The stop's own symbolication agrees too.
        let pc = d.read_registers().expect("registers").pc;
        let position = d.source_position(pc).expect("source position");
        assert_eq!(position.file.as_deref(), Some(line_file.as_str()));
        assert_eq!(position.line, Some(fixture.marker_line("in_b")));
    });
}

/// Breakpoints set with no process are pending, are resolved at the first stop of the
/// launched process, and are hit; address breakpoints are refused with no process.
#[test]
fn breakpoint_set_before_launch_is_resolved_at_entry()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let tag = t.tag();
        let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")));
        let id = set(&mut session, function("tick"));
        assert!(session.logical_breakpoints()[0].pending.is_some());
        assert_eq!(sites(&session, id).len(), 0);
        assert!(matches!(
            session.set_breakpoint(BreakpointLocation::Address(ferros_core::Address::new(0x1000))),
            Err(DebuggerError::NotAttached)
        ));

        launch_tagged(&mut session, &fixture, &tag);
        assert_eq!(session.state(), SessionState::Stopped);
        let breakpoint = session.logical_breakpoints()[0].clone();
        assert_eq!(breakpoint.pending, None, "{breakpoint:?}");
        assert_eq!(breakpoint.sites.len(), 1);
        let hit = continue_to_hit(&mut session);
        assert_eq!(hit.id, id);
        assert_eq!(
            hit.site.position.function.as_deref(),
            Some(qualified(&fixture, "tick").as_str())
        );

        // The breakpoint outlives the process and is resolved again in the next one.
        session.kill().expect("kill");
        assert!(session.logical_breakpoints()[0].pending.is_some());
        launch_tagged(&mut session, &fixture, &tag);
        assert_eq!(continue_to_hit(&mut session).id, id);
        session.kill().expect("kill");
    });
}

/// Launch `fixture` (looping long enough for the test) with the harness tag, so the
/// harness kills it if the test fails first.
fn launch_tagged(session: &mut Session, fixture: &Fixture, tag: &harness::Tag)
{
    let spec = LaunchSpec::new(fixture.path())
        .arg("100000")
        .arg(tag.as_str())
        .stdin(StdioSpec::Null)
        .stdout(StdioSpec::Null)
        .stderr(StdioSpec::Null);
    session.launch(&spec).expect("launch");
}

/// Runs `body` under [`harness::run`] after `ferros` is built and signed (see `cli.rs`).
fn run_cli<F>(body: F)
where
    F: FnOnce(&TestContext) + Send + 'static,
{
    let _ = transcript::ferros_bin();
    harness::run(body);
}

/// One batch transcript through the `ferros` binary: a `file:line` breakpoint set before
/// `run`, a function breakpoint, a stop banner with the location and the source text, a
/// list, disable, an unknown function, and delete. AC1, AC4, AC5.
#[test]
fn batch_transcript_with_source_breakpoints()
{
    run_cli(|t| {
        let fixture = fixture!("nested_calls");
        let in_b = fixture.marker_line("in_b");
        let in_c = fixture.marker_line("in_c");
        let source = std::fs::read_to_string(fixture.source_path()).expect("fixture source");
        let line_text = |line: u32| source.lines().nth(line as usize - 1).expect("line").trim_end().to_owned();
        let file = fixture.source_path().display().to_string();

        let tag = t.tag();
        let commands = vec![
            format!("break nested_calls.rs:{in_b}"),
            "break level_c".to_owned(),
            "break no_such_function".to_owned(),
            "breakpoints".to_owned(),
            "run".to_owned(),
            "disable 1".to_owned(),
            "continue".to_owned(),
            "delete 2".to_owned(),
            "breakpoints".to_owned(),
        ];
        let commands: Vec<&str> = commands.iter().map(String::as_str).collect();
        let run = transcript::launch_batch(&tag, &fixture, &[], &commands);
        assert_eq!(run.status.code(), Some(1), "{run:?}");
        assert_eq!(run.stderr, "error: no function matching `no_such_function`\n", "{run:?}");

        let level_b = qualified(&fixture, "level_b");
        let level_c = qualified(&fixture, "level_c");
        let b_text = line_text(in_b);
        let c_text = line_text(in_c);
        let expected = vec![
            "launched process <pid>".to_owned(),
            format!("(ferros) break nested_calls.rs:{in_b}"),
            format!("breakpoint 1 at 0x<hex>: {level_b} at {file}:{in_b}"),
            "(ferros) break level_c".to_owned(),
            format!("breakpoint 2 at 0x<hex>: {level_c} at {file}:{in_c}"),
            "(ferros) break no_such_function".to_owned(),
            "(ferros) breakpoints".to_owned(),
            format!("1 nested_calls.rs:{in_b} enabled hits=0"),
            format!("  0x<hex>: {level_b} at {file}:{in_b}"),
            "2 level_c enabled hits=0".to_owned(),
            format!("  0x<hex>: {level_c} at {file}:{in_c}"),
            "(ferros) run".to_owned(),
            "stopped: breakpoint 1 at 0x<hex> (thread <tid>)".to_owned(),
            format!("  {level_b} at {file}:{in_b}"),
            format!("  {in_b}  {b_text}"),
            "(ferros) disable 1".to_owned(),
            "disabled breakpoint 1".to_owned(),
            "(ferros) continue".to_owned(),
            "stopped: breakpoint 2 at 0x<hex> (thread <tid>)".to_owned(),
            format!("  {level_c} at {file}:{in_c}"),
            format!("  {in_c}  {c_text}"),
            "(ferros) delete 2".to_owned(),
            "deleted breakpoint 2".to_owned(),
            "(ferros) breakpoints".to_owned(),
            format!("1 nested_calls.rs:{in_b} disabled hits=1"),
            format!("  0x<hex>: {level_b} at {file}:{in_b}"),
            "killed process <pid>".to_owned(),
        ]
        .into_iter()
        .map(|line| line + "\n")
        .collect::<String>();
        assert_eq!(normalize(&run.stdout), expected, "{run:?}");
    });
}

/// Interactive `break <file>:<line>` in `loop_counter`: `continue` stops there on every
/// iteration, and `enable` re-arms it after `disable`.
#[test]
fn interactive_file_line_breakpoint_in_loop()
{
    run_cli(|t| {
        let fixture = fixture!("loop_counter");
        let line = fixture.marker_line("tick_body");
        let tag = t.tag();
        let mut ferros = Interactive::launch(&tag, &fixture, &["100000"]);
        let reply = ferros.send(&format!("break loop_counter.rs:{line}"));
        assert!(reply.starts_with("breakpoint 1 at 0x"), "{reply:?}");
        assert!(reply.ends_with(&format!("loop_counter.rs:{line}\n")), "{reply:?}");
        for _ in 0..2 {
            let reply = ferros.send("continue");
            assert!(reply.starts_with("stopped: breakpoint 1 at "), "{reply:?}");
            assert!(reply.contains(&format!("loop_counter.rs:{line}\n")), "{reply:?}");
        }
        assert_eq!(ferros.send("disable 1"), "disabled breakpoint 1\n");
        assert_eq!(ferros.send("enable 1"), "enabled breakpoint 1\n");
        let reply = ferros.send("continue");
        assert!(reply.starts_with("stopped: breakpoint 1 at "), "{reply:?}");
        let reply = ferros.send("breakpoints");
        assert!(
            reply.starts_with(&format!("1 loop_counter.rs:{line} enabled hits=3\n")),
            "{reply:?}"
        );
        let (status, _) = ferros.finish();
        assert!(status.success(), "{status}");
    });
}

/// Two breakpoints on one function share its trap. Disabled and then deleted, they leave
/// no trap entry, and a new breakpoint there traps again.
#[test]
fn shared_site_is_released_by_the_last_delete()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        let by_name = set(&mut d, function("level_c"));
        let by_path = set(&mut d, function(&qualified(&fixture, "level_c")));
        assert_eq!(sites(&d, by_name), sites(&d, by_path));
        assert_eq!(d.breakpoints().len(), 1);
        d.disable_logical_breakpoint(by_name).expect("disable");
        d.disable_logical_breakpoint(by_path).expect("disable");
        d.delete_breakpoint(by_name).expect("delete");
        d.delete_breakpoint(by_path).expect("delete");
        assert_eq!(d.breakpoints().len(), 0);

        let again = set(&mut d, function("level_c"));
        assert_eq!(continue_to_hit(&mut d).id, again);
    });
}

/// `break symbols.rs:<in_inlined>` is a line of the `#[inline(always)]` function
/// `always_inlined`, which only exists inlined into `main`. Every site names
/// `always_inlined` (the function whose source holds that line), not `main`, and the stop
/// reports the same position. Issue #108.
#[test]
fn line_in_an_inlined_function_names_that_function()
{
    harness::run(|t| {
        let fixture = fixture!("symbols");
        let mut d = t.launch(&fixture);
        let line = fixture.marker_line("in_inlined");
        let id = set(&mut d, at_marker(&fixture, "in_inlined"));
        let sites = sites(&d, id);
        assert!(sites.len() >= 2, "one site per inlined copy: {sites:?}");
        let expected = qualified(&fixture, "always_inlined");
        for site in &sites {
            assert_eq!(site.position.function.as_deref(), Some(expected.as_str()), "{site:?}");
            assert_eq!(site.position.line, Some(line), "{site:?}");
            assert!(
                site.position
                    .file
                    .as_deref()
                    .is_some_and(|file| file.ends_with("/fixtures/symbols.rs")),
                "{site:?}"
            );
        }
        let hit = continue_to_hit(&mut d);
        assert_eq!(hit.id, id);
        assert!(sites.contains(&hit.site), "{:?} not in {sites:?}", hit.site);
    });
}

/// Iterations for a `loop_counter` that must outlive the test (10 ms each).
const LONG_LOOP: &str = "100000";

/// The `ps` state of `pid`, or an empty string when it is gone.
fn process_state(pid: u32) -> String
{
    let output = std::process::Command::new("/bin/ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The process exists and is not stopped, sampled over a few `tick` calls: a trap left
/// in `tick` would stop or kill it.
fn assert_running(pid: u32, what: &str)
{
    for _ in 0..5 {
        let state = process_state(pid);
        assert!(
            !state.is_empty() && !state.starts_with('T') && !state.starts_with('Z'),
            "{what}: the process is stopped or gone (state {state:?})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn new_session() -> Session
{
    Session::new(Box::new(MacOSBackend::new().expect("MacOSBackend::new")))
}

/// A function breakpoint set with no process is resolved when the session attaches to a
/// running `loop_counter`, and the process stops there. Issue #110.
#[test]
fn breakpoint_set_before_attach_is_resolved_at_attach()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let target = t.spawn(&fixture, &[LONG_LOOP]);
        let mut session = new_session();
        let id = set(&mut session, function("tick"));
        assert!(session.logical_breakpoints()[0].pending.is_some());

        session.attach(target.pid()).expect("attach");
        assert_eq!(session.state(), SessionState::Stopped);
        let breakpoint = session.logical_breakpoints()[0].clone();
        assert_eq!(breakpoint.pending, None, "{breakpoint:?}");
        assert_eq!(breakpoint.sites.len(), 1, "{breakpoint:?}");

        let hit = continue_to_hit(&mut session);
        assert_eq!(hit.id, id);
        assert_eq!(hit.site, breakpoint.sites[0]);
        assert_eq!(
            hit.site.position.function.as_deref(),
            Some(qualified(&fixture, "tick").as_str())
        );
    });
}

/// Detaching from a process with a source breakpoint leaves it running without traps and
/// the breakpoint pending; attaching again resolves it and stops there. Issue #110.
#[test]
fn source_breakpoint_survives_detach_and_reattach()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut target = t.spawn(&fixture, &[LONG_LOOP]);
        let mut session = new_session();
        session.attach(target.pid()).expect("attach");
        let id = set(&mut session, at_marker(&fixture, "tick_body"));
        let [site] = sites(&session, id).try_into().expect("one site");
        assert_eq!(continue_to_hit(&mut session).site, site);

        session.detach().expect("detach");
        assert_eq!(session.state(), SessionState::NotStarted);
        let breakpoint = session.logical_breakpoints()[0].clone();
        assert!(breakpoint.pending.is_some(), "{breakpoint:?}");
        assert!(breakpoint.sites.is_empty(), "{breakpoint:?}");
        assert!(breakpoint.enabled);
        // `tick` runs every 10 ms: a trap left in it would stop or kill the process.
        assert_running(target.pid().0, "after detach");
        assert_eq!(target.try_wait().expect("try_wait"), None, "the process ended");

        session.attach(target.pid()).expect("attach again");
        let breakpoint = session.logical_breakpoints()[0].clone();
        assert_eq!(breakpoint.pending, None, "{breakpoint:?}");
        assert_eq!(breakpoint.sites, vec![site.clone()], "same address in the same process");
        let hit = continue_to_hit(&mut session);
        assert_eq!(hit.id, id);
        assert_eq!(hit.site, site);
        assert_eq!(session.logical_breakpoints()[0].hit_count, 2);
    });
}
