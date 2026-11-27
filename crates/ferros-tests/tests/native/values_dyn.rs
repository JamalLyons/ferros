//! Render trait objects from a stopped Rust program (issue #390).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_resolves_concrete_types_through_vtables()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_dyn");
        let break_at = format!("break values_dyn.rs:{}", fixture.marker_line("inspect"));
        let commands = vec![
            break_at.as_str(),
            "continue",
            "print debug",
            "print rectangle",
            "print square",
            "quit",
        ];
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let rendered = |name: &str| {
            run.stdout
                .lines()
                .find_map(|line| line.strip_prefix(name)?.split_once(" = ").map(|(_, value)| value))
                .unwrap_or_else(|| panic!("missing {name} in:\n{}", run.stdout))
        };
        assert_eq!(rendered("debug: "), "ferros_fixture_values_dyn::Reading { count: 11 }");
        assert_eq!(
            rendered("rectangle: "),
            "ferros_fixture_values_dyn::Rectangle { width: 3, height: 5 }"
        );
        assert_eq!(rendered("square: "), "ferros_fixture_values_dyn::Square { side: 7 }");
    });
}
