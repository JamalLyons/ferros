//! Raw argument and local values in physical and inline frames (issue #383).
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
        assert_eq!(argument.value.location, Some(ValueLocation::Register(0)));
        assert_eq!(argument.value.bytes, Err(Unavailable::RegisterUnknown(0)));
    });
}

#[test]
fn locals_command_follows_up_and_prints_raw_bytes()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_basic");
        let tag = t.tag();
        let break_at = format!("break values_basic.rs:{}", fixture.marker_line("callee_locals"));
        let commands = vec![break_at.as_str(), "continue", "locals", "up", "locals"];
        let run = transcript::launch_batch(&tag, &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let locals: Vec<_> = run.stdout.split("(ferros) locals\n").skip(1).collect();
        assert_eq!(locals.len(), 2, "{}", run.stdout);
        assert!(locals[0].contains("int_local: i32 = 0xefffffff\n"), "{}", run.stdout);
        assert!(
            locals[0].contains("float_local: f64 = 0x000000000000f43f\n"),
            "{}",
            run.stdout
        );
        assert!(locals[0].contains("bool_arg: bool = 0x01\n"), "{}", run.stdout);
        assert!(locals[0].contains("char_arg: char = 0xb5010000\n"), "{}", run.stdout);
        assert!(locals[1].contains("caller_int_arg: i32 = 0xaeffffff\n"), "{}", run.stdout);
        assert!(
            locals[1].contains("caller_float_arg: f64 = 0x000000000000e8bf\n"),
            "{}",
            run.stdout
        );
        assert!(locals[1].contains("caller_bool_local: bool = 0x01\n"), "{}", run.stdout);
        assert!(locals[1].contains("caller_char_local: char = 0x80f90100\n"), "{}", run.stdout);
        assert!(!run.stdout.contains("later_local") && !run.stdout.contains("later_caller_local"));
    });
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
