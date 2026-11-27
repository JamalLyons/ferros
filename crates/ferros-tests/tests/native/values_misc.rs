//! Unions, function pointers, raw slice pointers and vector iterators from a stopped Rust
//! program (issue #412).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_unions_function_symbols_raw_slices_and_iterators()
{
    const SAMPLES: &[(&str, &str)] = &[
        (
            "bits",
            "ferros_fixture_values_misc::Bits = ferros_fixture_values_misc::Bits { int: 1, float: 1e-45 }",
        ),
        (
            "word",
            "ferros_fixture_values_misc::Word = ferros_fixture_values_misc::Word { halves: \
             ferros_fixture_values_misc::Halves { low: 1, high: 2 }, raw: 131073, bytes: [1, 0, 2, 0] }",
        ),
        (
            "remaining",
            "alloc::vec::into_iter::IntoIter<u32, alloc::alloc::Global> = [2, 3]",
        ),
        ("empty", "alloc::vec::into_iter::IntoIter<u64, alloc::alloc::Global> = []"),
        // Only the slots not yet yielded, read as `u8`, not as `MaybeUninit` union members.
        ("array_iter", "core::array::iter::IntoIter<u8, 3> = [2, 3]"),
        (
            "uninit",
            "core::mem::maybe_uninit::MaybeUninit<u32> = MaybeUninit(<maybe uninit> 0x01020304)",
        ),
    ];
    // Checked below against addresses read from the same run.
    const OTHERS: &[&str] = &[
        "callback",
        "handler",
        "stray",
        "raw_slice",
        "null_slice",
        "data_ref",
        "raw_str",
        "raw_mut_str",
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_misc");
        let break_at = format!("break values_misc.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = SAMPLES
            .iter()
            .map(|(name, _)| *name)
            .chain(OTHERS.iter().copied())
            .map(|name| format!("print {name}"))
            .collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let rendered = |name: &str| {
            let prefix = format!("{name}: ");
            run.stdout
                .lines()
                .find_map(|line| line.strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("missing {name} in:\n{}\nstderr:\n{}", run.stdout, run.stderr))
                .to_owned()
        };
        for &(name, expected) in SAMPLES {
            assert_eq!(rendered(name), expected, "{name}");
        }

        // A function pointer names the function at its target.
        let callback = rendered("callback");
        let (address, symbol) = callback
            .strip_prefix("fn(u32, u32) -> u32 = ")
            .and_then(|value| value.split_once(' '))
            .unwrap_or_else(|| panic!("callback: {callback}"));
        assert_eq!(symbol, "<ferros_fixture_values_misc::add>", "callback: {callback}");
        let entry = u64::from_str_radix(address.trim_start_matches("0x"), 16).expect("hex address");
        assert_eq!(
            rendered("handler"),
            format!(
                "ferros_fixture_values_misc::Handler = ferros_fixture_values_misc::Handler {{ on_event: {address} \
                 <ferros_fixture_values_misc::add> }}"
            )
        );

        // No function starts inside `add`, so only the address prints.
        let stray = rendered("stray");
        assert_eq!(
            stray,
            format!(
                "ferros_fixture_values_misc::Callback = ferros_fixture_values_misc::Callback {{ raw: {}, call: {:#x} }}",
                entry + 4,
                entry + 4
            )
        );

        // A raw slice pointer shows its address and length, without reading the elements.
        let raw_slice = rendered("raw_slice");
        let data = rendered("data_ref");
        let data_address = data
            .strip_prefix("&[u8; 2] = ")
            .and_then(|value| value.split_once(" -> "))
            .map_or_else(|| panic!("data_ref: {data}"), |(address, _)| address);
        assert_eq!(raw_slice, format!("*const [u8] = {data_address} (len: 2)"));
        assert_eq!(rendered("null_slice"), "*mut [u8] = 0x0 (len: 0)");

        // A raw `str` pointer is shown the same way; both point at the same literal.
        let raw_str = rendered("raw_str");
        let str_address = raw_str
            .strip_prefix("*const str = 0x")
            .and_then(|value| value.strip_suffix(" (len: 8)"))
            .unwrap_or_else(|| panic!("raw_str: {raw_str}"));
        assert!(
            u64::from_str_radix(str_address, 16).is_ok_and(|address| address != 0),
            "raw_str: {raw_str}"
        );
        assert_eq!(rendered("raw_mut_str"), format!("*mut str = 0x{str_address} (len: 8)"));
    });
}
