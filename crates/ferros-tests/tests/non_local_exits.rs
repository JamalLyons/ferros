//! Stepping and step-resume across non-local exits by `longjmp` and `siglongjmp`
//! (issue #141), with the C program of the `long_jump` fixture.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{BreakpointLocation, BreakpointRequest, Session, StopEvent};
use ferros_tests::{Fixture, find_marker, fixture, harness};

const TIMEOUT: Duration = Duration::from_secs(5);
const SOURCE: &str = "long_jump.c";

/// The fixture's markers are in its C source, not its Rust stub.
fn location(fixture: &Fixture, marker: &str) -> BreakpointLocation
{
    let path = fixture.source_path().with_file_name("c").join(SOURCE);
    let source = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let line = find_marker(&source, marker).unwrap_or_else(|err| panic!("marker {marker} in {SOURCE}: {err:?}"));
    BreakpointLocation::Line {
        file: SOURCE.into(),
        line,
    }
}

fn stop(session: &mut Session) -> StopEvent
{
    session
        .wait(Some(TIMEOUT))
        .expect("wait")
        .expect("stopped within five seconds")
}

/// Run to `marker`, then remove its breakpoint.
fn start(session: &mut Session, fixture: &Fixture, marker: &str)
{
    let id = session.set_breakpoint(location(fixture, marker)).expect("source breakpoint");
    session.resume().expect("continue");
    assert!(matches!(stop(session), StopEvent::Breakpoint { .. }));
    assert_eq!(session.breakpoint_hit().expect("logical hit").id, id);
    session.delete_breakpoint(id).expect("remove starting breakpoint");
}

fn function(session: &mut Session) -> String
{
    let frames = session.stack_trace(64).expect("trace");
    frames[0]
        .symbol
        .as_ref()
        .map_or_else(String::new, |symbol| symbol.demangled().unwrap_or(symbol.raw()).to_owned())
}

/// After the step, the thread is in `main` and the program finishes from there.
fn finish_in_main(session: &mut Session, fixture: &Fixture)
{
    assert!(
        session.breakpoints().is_empty(),
        "private breakpoints leaked: {:?}",
        session.breakpoints()
    );
    let after = session.set_breakpoint(location(fixture, "after_jump")).expect("after jump");
    session.resume().expect("continue");
    assert!(matches!(stop(session), StopEvent::Breakpoint { .. }));
    assert_eq!(session.breakpoint_hit().expect("hit").id, after);
    session.delete_breakpoint(after).expect("remove");
    session.resume().expect("finish");
    assert_eq!(stop(session), StopEvent::Exited { code: 0 });
}

/// `next` over a call whose callee `longjmp`s past it to `main` never sees the call
/// return. It stops where the jump lands in `main` instead of running on.
#[test]
fn next_over_a_call_that_long_jumps_past_its_caller_stops_in_the_outer_frame()
{
    harness::run(|t| {
        let fixture = fixture!("long_jump");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "call_middle");
        let before = d.stack_trace(64).expect("trace").len();
        d.step_over().expect("next over a long-jumping call");
        let event = stop(&mut d);
        assert!(
            matches!(event, StopEvent::SingleStep { .. }),
            "expected a step stop, got {event:?}"
        );
        assert_eq!(function(&mut d), "main");
        assert_eq!(d.stack_trace(64).expect("trace").len(), before);
        finish_in_main(&mut d, &fixture);
        assert!(d.wait_for_stdout("long jump: 5", TIMEOUT));
    });
}

/// `finish` from a frame that `longjmp`s past its caller stops in the frame the jump
/// lands in.
#[test]
fn finish_from_a_frame_that_long_jumps_stops_in_the_outer_frame()
{
    harness::run(|t| {
        let fixture = fixture!("long_jump");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "in_jumper");
        let before = d.stack_trace(64).expect("trace").len();
        d.step_out().expect("finish from a long-jumping frame");
        let event = stop(&mut d);
        assert!(
            matches!(event, StopEvent::SingleStep { .. }),
            "expected a step stop, got {event:?}"
        );
        assert_eq!(function(&mut d), "main");
        assert_eq!(d.stack_trace(64).expect("trace").len() + 2, before);
        finish_in_main(&mut d, &fixture);
    });
}

/// A passed SIGUSR1's handler stops at a breakpoint before returning, then leaves by
/// `siglongjmp`. Its step-resume rendezvous is given up: when the thread next reaches the
/// original breakpoint in the same frame (a second `raise` from the same caller), that is
/// a real hit, reported and counted.
#[test]
fn a_handler_that_leaves_by_siglongjmp_gives_up_its_rendezvous()
{
    harness::run(|t| {
        let fixture = fixture!("long_jump");
        let mut d = t.launch_with_args(&fixture, &["--handler"]);
        d.resume().expect("run to signal");
        assert!(matches!(stop(&mut d), StopEvent::Signal { signal: 30, .. }));
        let pc = d.read_registers().expect("signal registers").pc;
        let original = d
            .add_breakpoint(BreakpointRequest::Software { address: pc })
            .expect("break at signal pc");
        let handler = d.set_breakpoint(location(&fixture, "in_handler")).expect("in handler");
        d.resume_with_signal().expect("pass SIGUSR1 through step-resume");
        assert!(matches!(stop(&mut d), StopEvent::Breakpoint { .. }));
        assert_eq!(d.breakpoint_hit().expect("handler hit").id, handler);
        d.delete_breakpoint(handler).expect("remove handler breakpoint");
        d.resume().expect("let the handler siglongjmp out");
        let event = stop(&mut d);
        assert_eq!(
            event,
            StopEvent::Breakpoint {
                thread: event.thread().expect("thread"),
                pc
            },
            "the second raise reaches the original breakpoint"
        );
        assert_eq!(d.breakpoint_info(original).expect("original breakpoint").hit_count, 1);
        d.remove_breakpoint(original).expect("remove original breakpoint");
        assert!(
            d.breakpoints().is_empty(),
            "private breakpoints leaked: {:?}",
            d.breakpoints()
        );
        d.resume().expect("finish");
        assert_eq!(stop(&mut d), StopEvent::Exited { code: 0 });
        assert!(d.wait_for_stdout("handler left by siglongjmp: true", TIMEOUT));
    });
}
