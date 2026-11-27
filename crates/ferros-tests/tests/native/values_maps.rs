//! Render map and set entries from a stopped Rust program (issue #389).
#![cfg(target_os = "macos")]

use std::collections::BTreeSet;

use ferros_tests::{fixture, harness, transcript};

fn entries(text: &str) -> BTreeSet<&str>
{
    let body = text.strip_prefix('{').and_then(|text| text.strip_suffix('}')).expect(text);
    body.split(", ").collect()
}

#[test]
fn print_shows_map_and_set_entries_with_limits()
{
    const PRINTS: &[&str] = &[
        "continue",
        "print hash",
        "print empty_hash",
        "print wide_hash",
        "print set",
        "print unit_set",
        "print tree",
        "print empty_tree",
        "print wide_tree",
        "quit",
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_maps");
        let break_at = format!("break values_maps.rs:{}", fixture.marker_line("inspect"));
        let mut commands = vec![break_at.as_str()];
        commands.extend(PRINTS);
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let render = |name: &str| {
            let prefix = format!("{name}: ");
            run.stdout
                .lines()
                .find_map(|line| line.strip_prefix(&prefix)?.split_once(" = ").map(|(_, value)| value))
                .unwrap_or_else(|| panic!("{name}:\n{}", run.stdout))
        };
        assert_eq!(
            entries(render("hash")),
            BTreeSet::from([r#""alpha": 1"#, r#""café": 2"#, r#""gamma": 4"#])
        );
        assert_eq!(entries(render("set")), BTreeSet::from(["5", "8", "13"]));
        assert_eq!(render("unit_set"), "{()}");
        assert_eq!(render("empty_hash"), "{}");
        assert_eq!(render("empty_tree"), "{}");

        let tree: Vec<_> = (0..15)
            .map(|n| format!("{n}: {:?}", if n % 2 == 0 { "even" } else { "odd" }))
            .collect();
        assert_eq!(render("tree"), format!("{{{}}}", tree.join(", ")));
        let tree_prefix: Vec<_> = (0..16).map(|n| format!(r#"{n}: "item""#)).collect();
        assert_eq!(render("wide_tree"), format!("{{{}, ...}} (len: 256)", tree_prefix.join(", ")));

        let wide_hash = render("wide_hash");
        let shown = wide_hash
            .strip_prefix('{')
            .and_then(|body| body.strip_suffix(", ...} (len: 40)"))
            .expect(wide_hash);
        let pairs: Vec<_> = shown.split(", ").collect();
        assert_eq!(pairs.len(), 16, "{wide_hash}");
        let mut keys = BTreeSet::new();
        for pair in pairs {
            let (key, value) = pair.split_once(": ").expect(pair);
            let key = key.parse::<u32>().expect(pair);
            assert!(key < 40 && keys.insert(key), "{wide_hash}");
            assert_eq!(value.parse::<u32>().expect(pair), key + 100);
        }
    });
}
