//! Argument and local values in physical and inline frames (issues #383 and #385).
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::values::location::{Unavailable, ValueLocation};
use ferros_core::values::scope::Variable;
use ferros_core::{BreakpointLocation, StopEvent};
use ferros_tests::{fixture, harness, transcript};

#[test]
fn reads_locals_and_arguments_in_the_callee_and_caller()
{
    const CALLEE_DECLARATIONS: &[&str] = &[
        "int_arg",
        "float_arg",
        "bool_arg",
        "char_arg",
        "int_local",
        "float_local",
        "bool_local",
        "char_local",
        "array_local",
    ];
    harness::run(|t| {
        let fixture = fixture!("values_basic");
        let mut d = t.launch(&fixture);
        d.set_breakpoint(BreakpointLocation::Line {
            file: "values_basic.rs".to_owned(),
            line: fixture.marker_line("callee_locals"),
        })
        .expect("break at callee locals");
        d.set_breakpoint(BreakpointLocation::Line {
            file: "values_basic.rs".to_owned(),
            line: fixture.marker_line("later_local"),
        })
        .expect("break inside the later lexical block");
        d.resume().expect("resume");
        let stop = d.wait(Some(Duration::from_secs(5))).expect("wait");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");

        let current = d.selected_frame_variables().expect("callee locals");
        let array = current.iter().find(|var| var.name == "array_local").expect("array local");
        assert_eq!(array.value.bytes.as_deref(), Ok(&[0x10, 0x32, 0x54, 0x76][..]));
        let names = current.iter().map(|var| var.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, CALLEE_DECLARATIONS);
        assert_value(&current, "int_arg", "i32", &37_i32.to_le_bytes());
        assert_value(&current, "float_arg", "f64", &6.5_f64.to_le_bytes());
        assert_value(&current, "bool_arg", "bool", &[1]);
        assert_value(&current, "char_arg", "char", &u32::from('Ƶ').to_le_bytes());
        assert_value(&current, "int_local", "i32", &(-17_i32).to_le_bytes());
        assert_value(&current, "float_local", "f64", &1.25_f64.to_le_bytes());
        assert_value(&current, "bool_local", "bool", &[0]);
        assert_value(&current, "char_local", "char", &u32::from('λ').to_le_bytes());
        assert!(!current.iter().any(|var| var.name == "later_local"), "{current:?}");

        let selected = d.select_frame(1).expect("select caller");
        assert_eq!(
            selected.location.and_then(|loc| loc.line),
            Some(fixture.marker_line("caller_call"))
        );
        let caller = d.selected_frame_variables().expect("caller locals");
        assert_value(&caller, "caller_int_arg", "i32", &(-82_i32).to_le_bytes());
        assert_value(&caller, "caller_float_arg", "f64", &(-0.75_f64).to_le_bytes());
        assert_value(&caller, "caller_bool_arg", "bool", &[0]);
        assert_value(&caller, "caller_char_arg", "char", &u32::from('雪').to_le_bytes());
        assert_value(&caller, "caller_int_local", "i32", &57_i32.to_le_bytes());
        assert_value(&caller, "caller_float_local", "f64", &(-2.5_f64).to_le_bytes());
        assert_value(&caller, "caller_bool_local", "bool", &[1]);
        assert_value(&caller, "caller_char_local", "char", &u32::from('🦀').to_le_bytes());
        assert!(!caller.iter().any(|var| var.name == "later_caller_local"), "{caller:?}");

        d.resume().expect("resume into the later block");
        let stop = d.wait(Some(Duration::from_secs(5))).expect("wait for the later block");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");
        let current = d.selected_frame_variables().expect("locals inside the later block");
        assert_value(&current, "later_local", "u32", &101_u32.to_le_bytes());
    });
}

#[test]
fn reads_only_the_selected_inline_or_physical_scope()
{
    harness::run(|t| {
        let fixture = fixture!("values_basic");
        let mut d = t.launch(&fixture);
        d.set_breakpoint(BreakpointLocation::Line {
            file: "values_basic.rs".to_owned(),
            line: fixture.marker_line("in_inline"),
        })
        .expect("break in inline function");
        d.resume().expect("resume");
        let stop = d.wait(Some(Duration::from_secs(5))).expect("wait");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");

        assert!(d.selected_stack_frame().expect("inline frame").kind.is_inlined());
        let inlined = d.selected_frame_variables().expect("inline locals");
        assert_value(&inlined, "inline_arg", "u32", &19_u32.to_le_bytes());
        assert_value(&inlined, "inline_local", "u32", &23_u32.to_le_bytes());
        assert!(!inlined.iter().any(|var| var.name == "owner_local"), "{inlined:?}");

        assert!(!d.select_frame(1).expect("physical frame").kind.is_inlined());
        let owner = d.selected_frame_variables().expect("owner locals");
        assert_value(&owner, "owner_local", "u32", &31_u32.to_le_bytes());
        assert!(!owner.iter().any(|var| var.name.starts_with("inline_")), "{owner:?}");
    });
}

#[test]
fn caller_saved_location_is_unavailable_in_a_real_caller()
{
    harness::run(|t| {
        let fixture = fixture!("values_basic");
        let mut debuggee = t.launch(&fixture);
        debuggee
            .set_breakpoint(BreakpointLocation::Function("values_register_stop".to_owned()))
            .expect("break in the optimized callee");
        debuggee.resume().expect("resume");
        let stop = debuggee.wait(Some(Duration::from_secs(5))).expect("wait");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");

        let caller = debuggee.select_frame(1).expect("select the optimized caller");
        assert!(
            caller
                .symbol
                .is_some_and(|symbol| symbol.raw().contains("values_register_caller"))
        );
        let variables = debuggee.selected_frame_variables().expect("optimized caller locals");
        let argument = variables
            .iter()
            .find(|var| var.name == "volatile_arg")
            .expect("argument described by the caller's location list");
        // ARM64 looks up return PC minus four. Clang's DW_OP_reg0 range includes
        // that whole call through call + 3 and ends at the return PC.
        // UnsupportedLocation indicates clang's DW_OP_entry_value range now covers
        // the lookup PC.
        assert_eq!(argument.value.location, Some(ValueLocation::Register(0)));
        assert_eq!(argument.value.bytes, Err(Unavailable::RegisterUnknown(0)));
    });
}

#[test]
fn locals_command_formats_values_in_the_selected_frame()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_basic");
        let tag = t.tag();
        let break_at = format!("break values_basic.rs:{}", fixture.marker_line("callee_locals"));
        let commands = vec![
            break_at.as_str(),
            "continue",
            "locals",
            "up",
            "locals",
            "print caller_int_arg",
            "quit",
        ];
        let run = transcript::launch_batch(&tag, &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let locals = command_output(&run.stdout, "locals");
        assert_eq!(
            locals,
            [
                concat!(
                    "int_arg: i32 = 37\n",
                    "float_arg: f64 = 6.5\n",
                    "bool_arg: bool = true\n",
                    "char_arg: char = 'Ƶ'\n",
                    "int_local: i32 = -17\n",
                    "float_local: f64 = 1.25\n",
                    "bool_local: bool = false\n",
                    "char_local: char = 'λ'\n",
                    "array_local: [u8; 4] = [16, 50, 84, 118]\n",
                ),
                concat!(
                    "caller_int_arg: i32 = -82\n",
                    "caller_float_arg: f64 = -0.75\n",
                    "caller_bool_arg: bool = false\n",
                    "caller_char_arg: char = '雪'\n",
                    "caller_int_local: i32 = 57\n",
                    "caller_float_local: f64 = -2.5\n",
                    "caller_bool_local: bool = true\n",
                    "caller_char_local: char = '🦀'\n",
                ),
            ],
            "{}",
            run.stdout
        );
        assert_eq!(
            command_output(&run.stdout, "print caller_int_arg"),
            ["caller_int_arg: i32 = -82\n"]
        );
        assert!(!run.stdout.contains("later_local") && !run.stdout.contains("later_caller_local"));
    });
}

#[test]
fn print_formats_aggregates_references_and_raw_pointers()
{
    const SAMPLES: &[(&str, &str)] = &[
        (
            "print nested",
            concat!(
                "nested: ferros_fixture_values_basic::Nested = ferros_fixture_values_basic::Nested ",
                "{ ready: true, inner: ferros_fixture_values_basic::Inner { count: -4 } }\n"
            ),
        ),
        ("print tuple_local", "tuple_local: (char, i16, bool) = ('λ', -3, false)\n"),
        ("print array_local", "array_local: [i16; 3] = [-2, 0, 7]\n"),
        ("print number", "number: i32 = -17\n"),
        ("print reference", "reference: &i32 = 0x<hex> -> -17\n"),
        ("print double_reference", "double_reference: &&i32 = 0x<hex> -> 0x<hex>\n"),
        ("print null_raw", "null_raw: *const i32 = 0x<hex>\n"),
        (
            "print large_array",
            "large_array: [u8; 32] = [42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, ...]\n",
        ),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_basic");
        let tag = t.tag();
        let break_at = format!("break values_basic.rs:{}", fixture.marker_line("format_locals"));
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(SAMPLES.iter().map(|&(command, _)| command));
        commands.extend(["locals", "quit"]);
        let run = transcript::launch_batch(&tag, &fixture, &[], &commands);
        assert!(
            run.status.success(),
            "status {:?}, stderr: {}\n{}",
            run.status,
            run.stderr,
            run.stdout
        );
        let normalized = transcript::normalize(&run.stdout);
        for &(command, expected) in SAMPLES {
            assert_eq!(command_output(&normalized, command), [expected], "{}", run.stdout);
        }
        let locals: String = SAMPLES.iter().map(|&(_, expected)| expected).collect();
        assert_eq!(command_output(&normalized, "locals"), [locals.as_str()]);
        assert_eq!(
            command_output(&run.stdout, "print null_raw"),
            ["null_raw: *const i32 = 0x0\n"]
        );
        assert!(command_output(&run.stdout, "locals")[0].contains("null_raw: *const i32 = 0x0\n"));
    });
}

fn command_output<'a>(stdout: &'a str, command: &str) -> Vec<&'a str>
{
    let prefix = format!("{command}\n");
    stdout
        .split("(ferros) ")
        .filter_map(|section| section.strip_prefix(&prefix))
        .collect()
}

fn assert_value(vars: &[Variable], name: &str, expected_type: &str, bytes: &[u8])
{
    let var = vars
        .iter()
        .find(|var| var.name == name)
        .unwrap_or_else(|| panic!("missing {name}: {vars:?}"));
    assert_eq!(var.type_name.as_deref(), Some(expected_type), "{var:?}");
    assert!(var.type_offset.is_some_and(|offset| offset.0 > 0), "{var:?}");
    assert!(var.die_offset.0 > 0, "{var:?}");
    assert!(matches!(var.value.location, Some(ValueLocation::Memory(_))), "{var:?}");
    assert_eq!(var.value.bytes.as_deref(), Ok(bytes), "{name}");
}
