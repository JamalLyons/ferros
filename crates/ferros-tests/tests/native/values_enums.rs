//! Render active enum variants from a stopped Rust program (issue #386).
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::values::format::FormatOptions;
use ferros_core::values::location::ValueLocation;
use ferros_core::{BreakpointLocation, StopEvent};
use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_active_variants_and_their_fields()
{
    const SAMPLES: &[(&str, &str)] = &[
        ("code_error", "Error"),
        ("code_ready", "Ready"),
        ("point", "Point"),
        ("circle", "Circle(12)"),
        ("rect", "Rect { width: 17, height: 23 }"),
        ("single", "Value(51)"),
        ("tagged_some", "Some(42)"),
        ("tagged_none", "None"),
        ("ref_some", "Some(0x<hex> -> -25)"),
        ("ref_none", "None"),
        ("box_some", "Some(88)"),
        ("box_none", "None"),
        ("nonzero_some", "Some(31)"),
        ("nonzero_none", "None"),
        (
            "offset_some",
            "Some(ferros_fixture_values_enums::Niche { prefix: 0, value: 56 })",
        ),
        ("offset_none", "None"),
        ("success", "Ok(61)"),
        ("failure", "Err(-71)"),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_enums");
        let break_at = format!("break values_enums.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = SAMPLES.iter().map(|(name, _)| format!("print {name}")).collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let normalized = transcript::normalize(&run.stdout);
        for &(name, expected) in SAMPLES {
            let prefix = format!("{name}: ");
            let rendered = normalized
                .lines()
                .find_map(|line| line.strip_prefix(&prefix)?.split_once(" = ").map(|(_, value)| value));
            assert_eq!(rendered, Some(expected), "{name}:\n{}", run.stdout);
        }
    });
}

#[test]
fn invalid_tags_report_the_raw_discriminant()
{
    const TAGS: &[(&str, &[u8], &str)] = &[
        ("circle", b"\xff", "<unknown variant: 0xff>"),
        ("code_error", b"\xfd\xff", "<unknown variant: 0xfffd>"),
    ];
    harness::run(|t| {
        let fixture = fixture!("values_enums");
        let mut d = t.launch(&fixture);
        d.set_breakpoint(BreakpointLocation::Line {
            file: "values_enums.rs".to_owned(),
            line: fixture.marker_line("inspect"),
        })
        .expect("break at the enum values");
        d.resume().expect("resume");
        let stop = d.wait(Some(Duration::from_secs(5))).expect("wait");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");

        let initial = d.selected_frame_variables().expect("fixture locals");
        for &(name, tag, _) in TAGS {
            let variable = initial.iter().find(|var| var.name == name).expect(name);
            let Some(ValueLocation::Memory(address)) = variable.value.location else {
                panic!("{name} should be stored in memory");
            };
            // Both explicit reprs put their discriminant first. The fixture stays stopped.
            d.write_memory(address, tag).expect("replace discriminant");
        }
        let corrupted = d.selected_frame_variables().expect("locals with invalid tags");
        for &(name, _, expected) in TAGS {
            let variable = corrupted.iter().find(|var| var.name == name).expect(name);
            assert_eq!(d.format_variable(variable, FormatOptions::default()).1, expected);
        }
    });
}
