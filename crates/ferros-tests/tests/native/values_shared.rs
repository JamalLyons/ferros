//! Render shared pointers and cells from a stopped Rust program (issue #388).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_shared_values_and_ownership_state()
{
    const SAMPLES: &[(&str, &str)] = &[
        ("rc", r#"Rc(strong: 2, weak: 1) "shared""#),
        ("arc", "Arc(strong: 1, weak: 1) [7, 8, 9] (len: 3, capacity: 3)"),
        ("rc_slice", "Rc(strong: 1, weak: 0) [4, 8]"),
        ("arc_str", r#"Arc(strong: 1, weak: 0) "café""#),
        ("cell", "Some(7)"),
        ("idle", "-6 (unborrowed)"),
        ("shared", "15 (shared borrows: 2)"),
        ("exclusive", "42 (mutably borrowed)"),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_shared");
        let break_at = format!("break values_shared.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = SAMPLES.iter().map(|(name, _)| format!("print {name}")).collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        for &(name, expected) in SAMPLES {
            let prefix = format!("{name}: ");
            let rendered = run
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix(&prefix)?.split_once(" = ").map(|(_, value)| value));
            assert_eq!(rendered, Some(expected), "{name}:\n{}", run.stdout);
        }
    });
}
