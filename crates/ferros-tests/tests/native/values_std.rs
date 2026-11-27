//! Render standard library values from a stopped Rust program (issue #387).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_standard_library_values()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let long_slice = format!("[{}...]", "42, ".repeat(16));
        let long_text = format!("\"{}\"...", "a".repeat(255));
        let units = format!("[(), (), ()] (len: 3, capacity: {})", usize::MAX);
        let samples = vec![
            ("text", r#""café\n\"quoted\"""#),
            ("empty", r#""""#),
            ("owned", r#""naïve\n""#),
            ("bytes", "[1, 2, 3] (len: 3, capacity: 8)"),
            ("strings", r#"["one", "two"] (len: 2, capacity: 2)"#),
            ("slice", "[-2, 5, 8]"),
            ("long_slice", &long_slice),
            ("empty_slice", "[]"),
            ("point", "ferros_fixture_values_std::Point { x: -7, y: 12 }"),
            ("boxed_array", "[3, 5]"),
            ("boxed_slice", "[6, 7]"),
            ("boxed_str", r#""boxed""#),
            ("units", &units),
            ("long_string", &long_text),
            ("long_text", &long_text),
        ];
        let fixture = fixture!("values_std");
        let break_at = format!("break values_std.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = samples.iter().map(|(name, _)| format!("print {name}")).collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        for (name, expected) in samples {
            let prefix = format!("{name}: ");
            let rendered = run
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix(&prefix)?.split_once(" = ").map(|(_, value)| value));
            assert_eq!(rendered, Some(expected), "{name}:\n{}", run.stdout);
        }
    });
}
