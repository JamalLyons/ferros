//! The command loop against the scripted `MockBackend`.

use std::io::Cursor;
use std::sync::atomic::AtomicBool;

use ferros_core::backend::LoadedImage;
use ferros_core::backend::mock::{MockBackend, MockCallKind, MockState, MockStop, arm64_registers};
use ferros_core::{Address, LaunchSpec, ProcessId, StopEvent, ThreadId};

use super::commands::{find, parse_address, parse_number};
use super::jobs::NoJobControl;
use super::{Debugger, Input, PROMPT, Target, history_path};

const THREAD: ThreadId = ThreadId(7);
const CODE: u64 = 0x1_0000;

fn mock() -> MockBackend
{
    let mut registers = arm64_registers();
    registers.pc = Address::new(CODE);
    MockBackend::new()
        .with_pid(ProcessId(42))
        .with_thread(THREAD, registers)
        .with_region(Address::new(CODE), "r-x", (0u8..64).collect::<Vec<_>>())
        .with_image(LoadedImage {
            path: "/bin/prog".into(),
            load_address: Address::new(CODE),
            uuid: None,
        })
}

struct Transcript
{
    ok: bool,
    out: String,
    err: String,
}

fn run_with(mock: &MockBackend, target: Target, input: Input<Cursor<Vec<u8>>>) -> Transcript
{
    let (mut debugger, _) = Debugger::start(Box::new(mock.clone()), target).expect("start");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let ok = super::run(
        &mut debugger,
        input,
        &mut out,
        &mut err,
        &AtomicBool::new(false),
        &mut NoJobControl,
    );
    Transcript {
        ok,
        out: String::from_utf8(out).expect("utf-8"),
        err: String::from_utf8(err).expect("utf-8"),
    }
}

fn batch(mock: &MockBackend, commands: &[&str]) -> Transcript
{
    let lines = commands.iter().map(|line| (*line).to_owned()).collect();
    run_with(mock, Target::Launch(LaunchSpec::new("/bin/prog")), Input::Lines(lines))
}

#[test]
fn numbers_and_addresses_parse()
{
    assert_eq!(parse_number("0x10"), Ok(16));
    assert_eq!(parse_number("0XfF"), Ok(255));
    assert_eq!(parse_number("42"), Ok(42));
    assert_eq!(parse_number("1_000"), Ok(1000));
    assert!(parse_number("0x").is_err());
    assert!(parse_number("ten").is_err());
    assert!(parse_number("-1").is_err());
    assert_eq!(parse_address("*0x1000"), Ok(Address::new(0x1000)));
    assert_eq!(parse_address("* 0x1000"), Ok(Address::new(0x1000)));
    assert_eq!(parse_address("4096"), Ok(Address::new(4096)));
    assert!(parse_address("*").is_err());
}

#[test]
fn aliases_find_their_commands()
{
    assert_eq!(find("c").map(|command| command.names()[0]), Some("continue"));
    assert_eq!(find("q").map(|command| command.names()[0]), Some("quit"));
    assert!(find("nope").is_none());
}

#[test]
fn every_command_name_is_unique()
{
    let mut names: Vec<&str> = super::commands::COMMANDS
        .iter()
        .flat_map(|command| command.names().iter().copied())
        .collect();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count, "a name or alias is registered twice");
}

#[test]
fn unknown_command_reports_an_error_and_the_next_command_runs()
{
    let mock = mock();
    let t = batch(&mock, &["frobnicate", "status"]);
    assert!(!t.ok);
    assert_eq!(t.err, "error: unknown command 'frobnicate' (try 'help')\n");
    assert!(
        t.out
            .contains("(ferros) frobnicate\n(ferros) status\nstatus: stopped (pid 42, thread 7)\n"),
        "{}",
        t.out
    );
}

#[test]
fn successful_batch_succeeds_and_skips_blank_lines_and_comments()
{
    let mock = mock();
    let t = batch(&mock, &["", "# a comment", "help"]);
    assert!(t.ok, "{}", t.err);
    assert!(t.out.starts_with("(ferros) help\n"), "{}", t.out);
    assert!(t.out.contains("continue "), "{}", t.out);
}

#[test]
fn continue_prints_the_exit()
{
    let mock = mock();
    mock.script(MockStop::new(StopEvent::Exited { code: 3 }));
    let t = batch(&mock, &["continue", "status"]);
    assert!(t.ok, "{}", t.err);
    assert!(
        t.out.contains("(ferros) continue\nprocess exited with status 3\n"),
        "{}",
        t.out
    );
    assert!(
        t.out.contains("status: exited\n  process exited with status 3\n"),
        "{}",
        t.out
    );
    // Nothing is left to kill at the end.
    assert!(!t.out.contains("killed process"), "{}", t.out);
}

#[test]
fn breakpoint_stop_names_the_breakpoint()
{
    let mock = mock();
    let address = CODE + 8;
    mock.script(MockStop::new(StopEvent::Breakpoint {
        thread: THREAD,
        pc: Address::new(address),
    }));
    let t = batch(&mock, &[&format!("break *{address:#x}"), "c", "breakpoints"]);
    assert!(t.ok, "{}", t.err);
    assert!(t.out.contains(&format!("breakpoint 1 at {address:#x}\n")), "{}", t.out);
    assert!(
        t.out.contains(&format!("stopped: breakpoint 1 at {address:#x} (thread 7)\n")),
        "{}",
        t.out
    );
    assert!(t.out.contains(&format!("1 *{address:#x} enabled hits=1\n")), "{}", t.out);
}

#[test]
fn memory_prints_a_hexdump_of_64_bytes_by_default()
{
    let mock = mock();
    let t = batch(&mock, &[&format!("memory {CODE:#x}"), &format!("x *{CODE} 2")]);
    assert!(t.ok, "{}", t.err);
    let rows: Vec<&str> = t.out.lines().filter(|line| line.starts_with("0x000000000001")).collect();
    assert_eq!(rows.len(), 5, "{}", t.out);
    assert!(rows[0].starts_with("0x0000000000010000  00 01 02 03"), "{}", t.out);
    assert!(rows[3].starts_with("0x0000000000010030  30 31 32"), "{}", t.out);
    assert!(rows[4].starts_with("0x0000000000010000  00 01   "), "{}", t.out);
}

#[test]
fn memory_read_failure_is_an_error()
{
    let mock = mock();
    let t = batch(&mock, &["memory 0x10"]);
    assert!(!t.ok);
    assert!(t.err.starts_with("error: "), "{}", t.err);
}

#[test]
fn registers_show_the_selected_thread()
{
    let mock = mock();
    let t = batch(&mock, &["registers", "threads", "thread 0"]);
    assert!(t.out.contains(&format!("pc   {CODE:#018x}\n")), "{}", t.out);
    assert!(t.out.contains("* 0 thread 7\n"), "{}", t.out);
    assert!(t.out.contains("selected thread 0 (thread 7)\n"), "{}", t.out);
    assert!(t.ok, "{}", t.err);
    let t = batch(&self::mock(), &["thread 1"]);
    assert_eq!(t.err, "error: no thread with index 1 (the process has 1)\n");
}

#[test]
fn images_print_load_address_and_path()
{
    let mock = mock();
    let t = batch(&mock, &["images"]);
    assert!(t.out.contains(&format!("{CODE:#x} /bin/prog\n")), "{}", t.out);
}

#[test]
fn end_of_input_kills_a_launched_process()
{
    let mock = mock();
    let t = batch(&mock, &["status"]);
    assert!(t.ok, "{}", t.err);
    assert!(t.out.ends_with("killed process 42\n"), "{}", t.out);
    assert_eq!(mock.state(), MockState::NoProcess);
    assert!(mock.calls().iter().any(|call| call.kind() == MockCallKind::Kill));
}

#[test]
fn quit_stops_reading_and_kills_a_launched_process()
{
    let mock = mock();
    let t = batch(&mock, &["quit", "frobnicate"]);
    assert!(t.ok, "{}", t.err);
    assert!(!t.out.contains("frobnicate"), "{}", t.out);
    assert!(t.out.ends_with("killed process 42\n"), "{}", t.out);
    assert_eq!(mock.state(), MockState::NoProcess);
}

#[test]
fn end_of_input_detaches_an_attached_process()
{
    let mock = mock();
    let t = run_with(
        &mock,
        Target::Attach(ProcessId(42)),
        Input::Lines(["status".to_owned()].into()),
    );
    assert!(t.ok, "{}", t.err);
    assert!(t.out.ends_with("detached from process 42\n"), "{}", t.out);
    let kinds: Vec<_> = mock.calls().iter().map(ferros_core::backend::mock::MockCall::kind).collect();
    assert!(kinds.contains(&MockCallKind::Detach), "{kinds:?}");
    assert!(!kinds.contains(&MockCallKind::Kill), "{kinds:?}");
}

#[test]
fn piped_input_prints_prompts_and_echoes()
{
    let mock = mock();
    let input = Input::Reader(Cursor::new(b"status\nbogus\n".to_vec()));
    let t = run_with(&mock, Target::Launch(LaunchSpec::new("/bin/prog")), input);
    assert!(!t.ok);
    assert!(
        t.out.starts_with(&format!(
            "{PROMPT}status\nstatus: stopped (pid 42, thread 7)\n  at entry\n{PROMPT}bogus\n{PROMPT}\n"
        )),
        "{}",
        t.out
    );
}

#[test]
fn run_relaunches_after_exit_stopped_at_entry()
{
    let mock = mock();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    let t = batch(&mock, &["run", "run", "status"]);
    assert!(t.ok, "{}", t.err);
    assert!(t.out.contains("(ferros) run\nprocess exited with status 0\n"), "{}", t.out);
    assert!(
        t.out
            .contains("launched process 42 (stopped at entry; address breakpoints were cleared)\n"),
        "{}",
        t.out
    );
    assert!(
        t.out.contains("status: stopped (pid 42, thread 7)\n  at entry\n"),
        "{}",
        t.out
    );
    let launches = mock.calls().iter().filter(|call| call.kind() == MockCallKind::Launch).count();
    assert_eq!(launches, 2);
}

#[test]
fn run_after_start_is_an_error()
{
    let mock = mock();
    mock.script(MockStop::new(StopEvent::Signal {
        thread: THREAD,
        signal: 11,
    }));
    let t = batch(&mock, &["continue", "run"]);
    assert!(t.out.contains("stopped: signal 11 (thread 7)\n"), "{}", t.out);
    assert_eq!(t.err, "error: the program has already started (use 'continue')\n");
}

#[test]
fn continue_does_not_pass_sigint_on()
{
    use ferros_core::ResumeRequest;
    use ferros_core::backend::mock::MockCall;

    let mock = mock();
    mock.script(MockStop::new(StopEvent::Signal {
        thread: THREAD,
        signal: 2,
    }));
    mock.script(MockStop::new(StopEvent::Signal {
        thread: THREAD,
        signal: 15,
    }));
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    let t = batch(&mock, &["c", "c", "c"]);
    assert!(t.ok, "{}", t.err);
    let resumes: Vec<ResumeRequest> = mock
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            MockCall::Resume(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(
        resumes,
        vec![
            ResumeRequest::continue_all(),
            ResumeRequest::continue_all(),
            ResumeRequest::continue_all().with_signal(THREAD, 15),
        ]
    );
}

#[test]
fn interrupt_flag_interrupts_a_running_process()
{
    let mock = mock();
    let (mut debugger, _) =
        Debugger::start(Box::new(mock.clone()), Target::Launch(LaunchSpec::new("/bin/prog"))).expect("start");
    debugger.session.resume().expect("resume");
    let flag = AtomicBool::new(true);
    let event = super::interrupt::wait_for_stop(&mut debugger.session, &flag, &NoJobControl).expect("wait");
    assert_eq!(event, StopEvent::Interrupted { thread: None });
}

#[test]
fn stepi_reports_the_new_pc()
{
    let mock = mock();
    mock.script(MockStop::new(StopEvent::SingleStep { thread: THREAD }).set_pc(THREAD, Address::new(CODE + 4)));
    let t = batch(&mock, &["stepi"]);
    assert!(t.ok, "{}", t.err);
    assert!(
        t.out.contains(&format!("stopped: step at {:#x} (thread 7)\n", CODE + 4)),
        "{}",
        t.out
    );
}

#[test]
fn run_reports_a_breakpoint_that_stays_pending()
{
    let mock = mock();
    mock.script(MockStop::new(StopEvent::Exited { code: 0 }));
    // The mock image has no debug info, so the function cannot be resolved.
    let t = batch(&mock, &["c", "break main", "run"]);
    assert!(
        t.out.contains("breakpoint 1 at main (pending: the program is not running)\n"),
        "{}",
        t.out
    );
    assert!(t.out.contains("breakpoint 1 at main is pending: "), "{}", t.out);
}

#[test]
fn history_path_prefers_ferros_history_then_home()
{
    use std::path::PathBuf;
    assert_eq!(
        history_path(Some("/tmp/h".into()), Some("/home/u".into())),
        Some(PathBuf::from("/tmp/h"))
    );
    assert_eq!(
        history_path(Some("".into()), Some("/home/u".into())),
        Some(PathBuf::from("/home/u/.ferros_history"))
    );
    assert_eq!(
        history_path(None, Some("/home/u".into())),
        Some(PathBuf::from("/home/u/.ferros_history"))
    );
    assert_eq!(history_path(None, None), None);
}
