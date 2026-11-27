//! Render std newtypes and niche wrappers as their inner value (issue #410).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_newtypes_as_their_inner_value()
{
    const SAMPLES: &[(&str, &str)] = &[
        ("non_zero", "12"),
        ("some_non_zero", "Some(12)"),
        ("no_non_zero", "None"),
        ("duration", "1.5s"),
        ("wrapping", "Wrapping(5)"),
        ("manually_drop", "6"),
        ("maybe_uninit", "MaybeUninit(<maybe uninit> 0x00000007)"),
        ("thread_id", "ThreadId(1)"),
        ("phantom", "PhantomData<u32>"),
        ("ids", "[1, 2, 3] (len: 3, capacity: 3)"),
        // Niche wrappers render as their integer wherever they appear.
        ("duration.nanos", "500000000"),
        ("ids.buf.inner.cap", "3"),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_newtypes");
        let break_at = format!("break values_newtypes.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = SAMPLES.iter().map(|(name, _)| format!("print {name}")).collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let mismatches: Vec<_> = SAMPLES
            .iter()
            .filter_map(|&(name, expected)| {
                let prefix = format!("{name}: ");
                let rendered = run
                    .stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(&prefix)?.split_once(" = ").map(|(_, value)| value));
                (rendered != Some(expected)).then(|| format!("{name}: expected {expected:?}, got {rendered:?}"))
            })
            .collect();
        assert!(mismatches.is_empty(), "{}\n{}", mismatches.join("\n"), run.stdout);
    });
}
