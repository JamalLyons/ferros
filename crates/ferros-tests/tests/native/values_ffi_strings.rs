//! Render FFI and OS strings from a stopped Rust program (issue #411).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_ffi_and_os_strings_as_text()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let long_os = format!("\"\\xff{}\"...", "a".repeat(254));
        let samples = vec![
            ("c_string", r#"c"cstr""#),
            ("c_str", r#"c"borrowed""#),
            ("os_string", r#""os""#),
            ("os_str", r#""os\tstr""#),
            ("path_buf", r#""/tmp/x""#),
            ("path", r#""/tmp/café""#),
            ("invalid", r#""a\xffb\xc3""#),
            ("boxed_path", r#""/boxed""#),
            ("long_os", &long_os),
            ("text", r#""plain""#),
            ("owned", r#""owned\n""#),
            ("boxed_str", r#""boxed""#),
        ];
        let fixture = fixture!("values_ffi_strings");
        let break_at = format!("break values_ffi_strings.rs:{}", fixture.marker_line("inspect"));
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
