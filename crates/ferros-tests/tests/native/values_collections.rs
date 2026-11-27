//! Render ring buffers, linked lists, binary heaps and B-tree sets from a stopped Rust
//! program (issue #408).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_collection_elements_in_logical_order()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let prefix: Vec<_> = (0..16).map(|n| n.to_string()).collect();
        let long_deque = format!("[{}, ...] (len: 20, capacity: 20)", prefix.join(", "));
        let long_list = format!("[{}, ...] (len: 20)", prefix.join(", "));
        let samples = vec![
            ("wrapped", "[20, 30, 40, 50] (len: 4, capacity: 4)"),
            ("pushed_front", "[1, 2, 3] (len: 3, capacity: 4)"),
            ("empty_deque", "[] (len: 0, capacity: 0)"),
            ("long_deque", &long_deque),
            ("list", "['a', 'b', 'c']"),
            ("empty_list", "[]"),
            ("long_list", &long_list),
            ("heap", "[3, 1, 2] (len: 3, capacity: 4)"),
            ("empty_heap", "[] (len: 0, capacity: 0)"),
            ("set", "{5, 6}"),
            ("empty_set", "{}"),
            ("map", "{5: 50, 6: 60}"),
        ];
        let fixture = fixture!("values_collections");
        let break_at = format!("break values_collections.rs:{}", fixture.marker_line("inspect"));
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
