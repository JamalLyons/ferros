//! Source stepping and signal-safe breakpoint step-resume (issue #39).
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::{BreakpointLocation, LogicalBreakpointId, Session, StopEvent};
use ferros_tests::{Fixture, fixture, harness};

const TIMEOUT: Duration = Duration::from_secs(5);

fn location(fixture: &Fixture, marker: &str) -> BreakpointLocation
{
    BreakpointLocation::Line {
        file: format!("{}.rs", fixture.name()),
        line: fixture.marker_line(marker),
    }
}

fn stop(session: &mut Session) -> StopEvent
{
    session
        .wait(Some(TIMEOUT))
        .expect("wait")
        .expect("step completed within five seconds")
}

fn at(session: &mut Session, fixture: &Fixture, marker: &str) -> LogicalBreakpointId
{
    let id = session.set_breakpoint(location(fixture, marker)).expect("source breakpoint");
    session.resume().expect("continue");
    assert!(matches!(stop(session), StopEvent::Breakpoint { .. }));
    assert_eq!(session.breakpoint_hit().expect("logical hit").id, id);
    id
}

fn start(session: &mut Session, fixture: &Fixture, marker: &str)
{
    let id = at(session, fixture, marker);
    session.delete_breakpoint(id).expect("remove starting breakpoint");
}

fn line(session: &mut Session) -> u32
{
    let pc = session.read_registers().expect("registers").pc;
    session.source_position(pc).expect("position").line.expect("source line")
}

fn stepped(session: &mut Session)
{
    let event = stop(session);
    assert!(
        matches!(event, StopEvent::SingleStep { .. }),
        "expected source step, got {event:?}"
    );
}

#[test]
fn next_moves_to_the_following_marked_line()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "in_b");
        d.step_over().expect("next");
        stepped(&mut d);
        assert_eq!(line(&mut d), fixture.marker_line("call_c"));
    });
}

#[test]
fn next_over_a_call_never_stops_inside_it()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "call_a");
        d.step_over().expect("next over level_a");
        stepped(&mut d);
        assert_eq!(line(&mut d), fixture.marker_line("after_calls"));
        assert!(d.breakpoints().is_empty(), "private return breakpoint leaked");
    });
}

#[test]
fn step_enters_the_callee_after_its_prologue()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "call_a");
        d.step_into().expect("step into level_a");
        stepped(&mut d);
        assert_eq!(line(&mut d), fixture.marker_line("in_a"));
        let pc = d.read_registers().expect("registers").pc;
        let entry = d
            .set_breakpoint(BreakpointLocation::Function("level_a".into()))
            .expect("entry");
        let site = &d
            .logical_breakpoints()
            .iter()
            .find(|bp| bp.id == entry)
            .expect("breakpoint")
            .sites[0];
        assert_eq!(pc, site.address, "same post-prologue pc as a function breakpoint");
    });
}

/// `step` at `call` stops on `inside`, at the same post-prologue pc a breakpoint on
/// that line gets: the callee is found by its address, not its name.
fn step_into_stops_after_the_prologue(call: &'static str, inside: &'static str)
{
    harness::run(move |t| {
        let fixture = fixture!("step_targets");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, call);
        d.step_into().expect("step into the call");
        stepped(&mut d);
        assert_eq!(line(&mut d), fixture.marker_line(inside), "stopped in the callee from {call}");
        let pc = d.read_registers().expect("registers").pc;
        let bp = d.set_breakpoint(location(&fixture, inside)).expect("callee line");
        let sites = &d
            .logical_breakpoints()
            .iter()
            .find(|logical| logical.id == bp)
            .expect("breakpoint")
            .sites;
        assert!(
            sites.iter().any(|site| site.address == pc),
            "{pc} is not the line's statement address {sites:?}"
        );
    });
}

/// Issue #120 (3): a trait-impl method whose demangled name does not parse as a path.
#[test]
fn step_enters_a_trait_method_after_its_prologue()
{
    step_into_stops_after_the_prologue("call_trait", "in_trait");
}

/// Issue #120 (3): a closure whose demangled name does not parse as a path.
#[test]
fn step_enters_a_closure_after_its_prologue()
{
    step_into_stops_after_the_prologue("call_closure", "in_closure");
}

#[test]
fn finish_returns_to_the_caller()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "in_c");
        let before = d.stack_trace(64).expect("trace").len();
        d.step_out().expect("finish");
        stepped(&mut d);
        assert_eq!(d.stack_trace(64).expect("trace").len(), before - 1);
        let pc = d.read_registers().expect("registers").pc;
        assert!(
            d.source_position(pc)
                .expect("position")
                .function
                .expect("function")
                .ends_with("::level_b")
        );
    });
}

#[test]
fn finish_from_recursive_depth_three_stops_at_depth_two()
{
    harness::run(|t| {
        let fixture = fixture!("recursion");
        let mut d = t.launch_with_args(&fixture, &["3"]);
        start(&mut d, &fixture, "bottom");
        let trace = d.stack_trace(64).expect("trace");
        let count = |frames: &[ferros_core::StackFrame]| {
            frames
                .iter()
                .filter(|frame| {
                    frame
                        .symbol
                        .as_ref()
                        .is_some_and(|symbol| symbol.display_name().ends_with("::descend"))
                })
                .count()
        };
        assert_eq!(count(&trace), 4, "depth zero plus three recursive calls");
        d.step_out().expect("finish from depth three");
        stepped(&mut d);
        let after = d.stack_trace(64).expect("caller trace");
        assert_eq!(after.len(), trace.len() - 1);
        assert_eq!(count(&after), 3);
        // Finish again at the repeated return address: the frame guard must skip
        // the deeper recursion sharing that address.
        d.step_out().expect("finish from depth two");
        stepped(&mut d);
        assert_eq!(count(&d.stack_trace(64).expect("trace")), 2);
    });
}

#[test]
fn next_stops_at_a_user_breakpoint_inside_the_call()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "call_a");
        let id = d.set_breakpoint(location(&fixture, "in_c")).expect("callee breakpoint");
        d.step_over().expect("next");
        assert!(matches!(stop(&mut d), StopEvent::Breakpoint { .. }));
        let hit = d.breakpoint_hit().expect("user breakpoint hit");
        assert_eq!(hit.id, id);
        assert_eq!(hit.site.position.line, Some(fixture.marker_line("in_c")));
        assert_eq!(d.breakpoints().len(), 1, "private return breakpoint leaked");
    });
}

#[test]
fn ten_next_commands_follow_loop_lines_in_order()
{
    harness::run(|t| {
        let fixture = fixture!("loop_counter");
        let mut d = t.launch_with_args(&fixture, &["5"]);
        start(&mut d, &fixture, "loop_call");
        let mut visited = Vec::new();
        for _ in 0..10 {
            d.step_over().expect("next");
            stepped(&mut d);
            let pc = d.read_registers().expect("registers").pc;
            let position = d.source_position(pc).expect("source position");
            assert!(position.function.expect("function").ends_with("::main"));
            visited.push(position.line.expect("line"));
        }
        let markers = vec![
            "loop_progress",
            "loop_sleep",
            "loop_header",
            "loop_call",
            "loop_progress",
            "loop_sleep",
            "loop_header",
            "loop_call",
            "loop_progress",
            "loop_sleep",
        ];
        let expected: Vec<_> = markers.iter().map(|marker| fixture.marker_line(marker)).collect();
        assert_eq!(visited, expected);
    });
}

#[test]
fn stepping_past_main_reports_exit()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "call_exit");
        d.step_over().expect("next over exit");
        assert_eq!(stop(&mut d), StopEvent::Exited { code: 0 });
    });
}

/// A native signal stop saves the pc of the instruction still to execute. Putting a
/// breakpoint there recreates the R3 step-resume state deterministically, without
/// racing an externally sent signal against a single instruction. The handler must
/// run, return to the saved pc, and execute that instruction without a user hit.
#[test]
fn signal_handler_return_does_not_count_the_step_resume_breakpoint()
{
    harness::run(|t| {
        let fixture = fixture!("signal_handler");
        let mut d = t.launch_with_args(&fixture, &["--step-resume"]);
        d.resume().expect("run to signal");
        assert!(matches!(stop(&mut d), StopEvent::Signal { signal: 30, .. }));
        let pc = d.read_registers().expect("signal registers").pc;
        let id = d
            .add_breakpoint(ferros_core::BreakpointRequest::Software { address: pc })
            .expect("break at signal pc");
        let after = d.set_breakpoint(location(&fixture, "after_signal")).expect("after handler");
        d.resume_with_signal().expect("pass SIGUSR1 through step-resume");
        assert!(matches!(stop(&mut d), StopEvent::Breakpoint { .. }));
        assert_eq!(d.breakpoint_info(id).expect("original breakpoint").hit_count, 0);
        assert_eq!(d.breakpoint_hit().expect("after handler hit").id, after);
        d.remove_breakpoint(id).expect("remove original breakpoint");
        d.delete_breakpoint(after).expect("remove after breakpoint");
        d.resume().expect("finish");
        assert_eq!(stop(&mut d), StopEvent::Exited { code: 0 });
        assert!(d.wait_for_stdout("signal handled: true", TIMEOUT));
    });
}

/// Complement the real handler test with the exact ordering: a user hit, a signal
/// interrupting the instruction step before it executes, then handler return to
/// that same pc. This ordering cannot be forced by sending a signal to a stopped
/// process: the kernel may deliver it after the instruction completes instead.
#[test]
fn signal_during_breakpoint_step_does_not_report_or_count_a_second_hit()
{
    use ferros_core::backend::mock::{MockBackend, MockStop, arm64_registers};
    use ferros_core::backend::{ResumeRequest, ThreadAction};
    use ferros_core::{Address, BreakpointRequest, LaunchSpec, ThreadId};

    harness::run(|_| {
        let thread = ThreadId(1);
        let address = Address::new(0x1000);
        let next = address + 4;
        let mut registers = arm64_registers();
        registers.pc = address + 16;
        registers.sp = Address::new(0x8000);
        let mock = MockBackend::new()
            .with_thread(thread, registers)
            .with_region(address, "r-x", vec![0; 64]);
        let mut session = Session::new(Box::new(mock.clone()));
        session.launch(&LaunchSpec::new("scripted-signal-step")).expect("launch mock");
        let id = session
            .add_breakpoint(BreakpointRequest::Software { address })
            .expect("breakpoint");
        mock.script(
            MockStop::new(StopEvent::Breakpoint { thread, pc: address })
                .set_pc(thread, address)
                .expect_resume(ResumeRequest::continue_all()),
        );
        session.resume().expect("run to user hit");
        assert!(matches!(stop(&mut session), StopEvent::Breakpoint { .. }));
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
        mock.script(MockStop::new(StopEvent::Signal { thread, signal: 30 }).expect_resume(ResumeRequest::step(thread)));
        session.resume().expect("signal interrupts original instruction");
        assert!(matches!(stop(&mut session), StopEvent::Signal { .. }));
        mock.script(
            MockStop::new(StopEvent::Breakpoint { thread, pc: address })
                .set_pc(thread, address)
                .expect_resume(
                    ResumeRequest::all(ThreadAction::Suspend)
                        .with_thread(thread, ThreadAction::Continue)
                        .with_signal(thread, 30),
                ),
        );
        mock.script(
            MockStop::new(StopEvent::SingleStep { thread })
                .set_pc(thread, next)
                .expect_resume(ResumeRequest::step(thread)),
        );
        mock.script(
            MockStop::new(StopEvent::Interrupted { thread: Some(thread) }).expect_resume(ResumeRequest::continue_all()),
        );
        session
            .resume_with_signal()
            .expect("handler returns, original instruction executes");
        assert_eq!(stop(&mut session), StopEvent::Interrupted { thread: Some(thread) });
        assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
        assert_eq!(mock.peek_registers(thread).expect("registers").pc, next);
    });
}

#[test]
fn next_through_the_return_from_main_eventually_reports_exit()
{
    harness::run(|t| {
        let fixture = fixture!("hello");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "hello_print");
        for _ in 0..32 {
            d.step_over().expect("next through main return");
            match stop(&mut d) {
                StopEvent::Exited { code: 0 } => return,
                StopEvent::SingleStep { .. } => {}
                other => panic!("unexpected stop: {other:?}"),
            }
        }
        panic!("stepping past main never reported exit");
    });
}

#[test]
fn next_over_recursion_ignores_deeper_frames_at_the_same_return_pc()
{
    harness::run(|t| {
        let fixture = fixture!("recursion");
        let mut d = t.launch_with_args(&fixture, &["3"]);
        start(&mut d, &fixture, "recurse");
        let before = d
            .stack_trace(64)
            .expect("trace")
            .iter()
            .filter(|frame| !frame.is_inlined())
            .count();
        d.step_over().expect("next over recursion");
        stepped(&mut d);
        assert_eq!(line(&mut d), fixture.marker_line("after_recurse"));
        assert_eq!(
            d.stack_trace(64)
                .expect("trace")
                .iter()
                .filter(|frame| !frame.is_inlined())
                .count(),
            before
        );
        assert!(d.breakpoints().is_empty());
    });
}

#[test]
fn step_reports_a_user_breakpoint_on_the_callees_first_line()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "call_a");
        let id = d.set_breakpoint(location(&fixture, "in_a")).expect("callee breakpoint");
        d.step_into().expect("step");
        assert!(matches!(stop(&mut d), StopEvent::Breakpoint { .. }));
        assert_eq!(d.breakpoint_hit().expect("callee hit").id, id);
        assert_eq!(d.logical_breakpoints()[0].hit_count, 1);
    });
}

#[test]
fn finish_reports_a_user_breakpoint_at_its_return_address()
{
    harness::run(|t| {
        let fixture = fixture!("nested_calls");
        let mut d = t.launch(&fixture);
        start(&mut d, &fixture, "in_c");
        let address = d
            .stack_trace(64)
            .expect("trace")
            .iter()
            .find(|frame| !frame.is_inlined())
            .expect("physical frame")
            .return_address
            .expect("return pc");
        let id = d
            .add_breakpoint(ferros_core::BreakpointRequest::Software { address })
            .expect("return breakpoint");
        d.step_out().expect("finish");
        assert!(matches!(stop(&mut d), StopEvent::Breakpoint { pc, .. } if pc == address));
        assert_eq!(d.breakpoint_info(id).expect("user breakpoint").hit_count, 1);
        assert!(d.breakpoint_info(id).expect("user breakpoint").enabled);
    });
}

#[test]
fn next_holds_other_threads_even_when_their_pc_is_a_user_breakpoint()
{
    harness::run(|t| {
        let fixture = fixture!("threads");
        let mut d = t.launch(&fixture);
        let main = d.selected_thread().expect("main thread");
        start(&mut d, &fixture, "worker_body");
        // Both workers may have hit the initial trap in the same exception batch.
        // Drain that legitimate retained user stop before starting a new command.
        while d
            .wait(Some(Duration::from_millis(20)))
            .expect("drain initial stops")
            .is_some()
        {}
        let selected = d.selected_thread().expect("selected worker");
        let others: Vec<_> = d
            .threads()
            .expect("threads")
            .into_iter()
            .filter(|thread| *thread != selected)
            .map(|thread| (thread, d.read_registers_for(thread).expect("peer registers")))
            .collect();
        // Adding a trap at a suspended peer's pc must not single-step that peer as
        // part of the selected worker's call-skipping request.
        for (thread, registers) in &others {
            if *thread != main {
                continue;
            }
            d.add_breakpoint(ferros_core::BreakpointRequest::Software { address: registers.pc })
                .expect("peer breakpoint");
        }
        d.step_over().expect("next");
        stepped(&mut d);
        // The next line includes a sleep call, so this also checks the temporary
        // return-breakpoint phase, rather than only hardware single-step.
        d.step_over().expect("next over sleep");
        stepped(&mut d);
        for (thread, before) in others {
            let after = d.read_registers_for(thread).expect("peer registers");
            assert_eq!(after.pc, before.pc, "suspended peer {thread:?} moved");
            assert_eq!(after.sp, before.sp, "suspended peer {thread:?} changed frame");
        }
    });
}

/// `next` on a line that never ends (a loop calling a function forever) keeps stepping;
/// `interrupt` must stop it. The engine is driven directly, so no terminal `SIGINT` reaches
/// the program, as when attached: only the session's own interrupt can end the step.
#[test]
fn interrupt_ends_a_next_that_never_leaves_its_line()
{
    harness::run(|t| {
        let fixture = fixture!("hot_loop");
        let mut d = t.launch_with_args(&fixture, &["one-line"]);
        start(&mut d, &fixture, "spin_line");
        for _ in 0..5 {
            d.step_over().expect("next");
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(
                d.wait(Some(Duration::ZERO)).expect("still stepping"),
                None,
                "next left the endless line"
            );
            d.interrupt().expect("interrupt");
            let event = stop(&mut d);
            assert!(
                matches!(event, StopEvent::Interrupted { .. }),
                "interrupt must end the step, got {event:?}"
            );
            // While `next` runs over a call, the interrupt can stop the thread inside the
            // callee; `finish` brings it back to the loop line for the next round.
            // It can be two calls deep (`spin_site` calling std's atomic add).
            for _ in 0..4 {
                if line(&mut d) == fixture.marker_line("spin_line") {
                    break;
                }
                d.step_out().expect("finish");
                stepped(&mut d);
            }
            assert_eq!(line(&mut d), fixture.marker_line("spin_line"), "still on the endless line");
        }
    });
}
