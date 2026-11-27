//! `print <path>`: field access, indexing and dereference from a stopped Rust program
//! (issue #391).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_follows_fields_indexes_and_dereferences()
{
    const SAMPLES: &[(&str, &str)] = &[
        ("p.inner.x", "i32 = 5"),
        (
            "p.inner",
            "ferros_fixture_values_paths::Inner = ferros_fixture_values_paths::Inner { x: 5, y: -9 }",
        ),
        ("p.label", r#"&str = "origin""#),
        ("v[2]", "u32 = 30"),
        ("*boxed", "i64 = 77"),
        ("t.1", "u16 = 314"),
        ("t.0", "char = 'q'"),
        ("(*r).name", r#"alloc::string::String = "ferris""#),
        ("r.id", "u16 = 3"),
        ("(**rr).id", "u16 = 3"),
        ("rr.name", r#"alloc::string::String = "ferris""#),
        ("arr[1]", "i8 = -2"),
        ("slice[0]", "u32 = 20"),
        ("(*raw).y", "i32 = -9"),
        ("wrapper.0", "u8 = 4"),
        ("wrapper.1.y", "i32 = 16"),
        ("boxed_point.x", "i32 = -1"),
        ("(*boxed_point).y", "i32 = 2"),
        ("grid[1][0]", "u8 = 3"),
        ("( * r ) . id", "u16 = 3"),
    ];
    const FAILURES: &[(&str, &str)] = &[
        (
            "v[4]",
            "cannot evaluate 'v[4]': `v[4]`: index 4 is out of bounds for length 4",
        ),
        (
            "arr[3]",
            "cannot evaluate 'arr[3]': `arr[3]`: index 3 is out of bounds for length 3",
        ),
        (
            "empty[0]",
            "cannot evaluate 'empty[0]': `empty[0]`: index 0 is out of bounds for length 0",
        ),
        (
            "p.inner.z",
            "cannot evaluate 'p.inner.z': `p.inner.z`: no field `z` in ferros_fixture_values_paths::Inner",
        ),
        ("t.3", "cannot evaluate 't.3': `t.3`: no field `3` in (char, u16, bool)"),
        ("*null", "cannot evaluate '*null': `*null`: null pointer"),
        ("(*null).x", "cannot evaluate '(*null).x': `*null`: null pointer"),
        (
            "raw.x",
            "cannot evaluate 'raw.x': `raw.x`: *const ferros_fixture_values_paths::Inner has no fields",
        ),
        ("*count", "cannot evaluate '*count': `*count`: cannot dereference u32"),
        ("count[0]", "cannot evaluate 'count[0]': `count[0]`: cannot index u32"),
        ("missing.x", "no variable named 'missing' in the selected frame"),
        (
            "v.len()",
            "invalid path 'v.len()': unexpected `(` at column 6: function calls are not supported",
        ),
        (
            "v[1] + 1",
            "invalid path 'v[1] + 1': unexpected `+` at column 6: operators are not supported",
        ),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_paths");
        let break_at = format!("break values_paths.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = SAMPLES
            .iter()
            .chain(FAILURES)
            .map(|(path, _)| format!("print {path}"))
            .collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        for &(path, expected) in SAMPLES {
            let display = path.replace(' ', "");
            let prefix = format!("{display}: ");
            let rendered = run.stdout.lines().find_map(|line| line.strip_prefix(&prefix));
            assert_eq!(rendered, Some(expected), "{path}:\n{}\nstderr:\n{}", run.stdout, run.stderr);
        }
        let errors: Vec<_> = run.stderr.lines().filter_map(|line| line.strip_prefix("error: ")).collect();
        let expected: Vec<_> = FAILURES.iter().map(|(_, message)| *message).collect();
        assert_eq!(errors, expected, "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    });
}
