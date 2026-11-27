//! Statics and globals: `print` of a static by name and `globals [filter]` (issue #392).
#![cfg(target_os = "macos")]

use std::fmt::Write as _;
use std::time::Duration;

use ferros_core::{Address, BreakpointLocation, StopEvent};
use ferros_tests::{fixture, harness, transcript};

const PREFIX: &str = "ferros_fixture_values_statics::";
const COUNTER_TYPE: &str = "core::sync::atomic::Atomic<u64>";

/// The fixture's statics in `globals` order, with their types and values.
const STATICS: &[(&str, &str, &str)] = &[
    (
        "COUNTER",
        COUNTER_TYPE,
        "core::sync::atomic::Atomic<u64> { v: core::cell::UnsafeCell<core::sync::atomic::private::Align8<u64>> { value: \
         core::sync::atomic::private::Align8<u64>(42) } }",
    ),
    ("NAME", "&str", r#""ferris""#),
    ("TABLE", "[u32; 4]", "[1, 2, 3, 4]"),
    ("calls::CALLS", "u8", "7"),
    ("config::LIMIT", "u16", "512"),
];

#[test]
fn print_and_globals_read_statics_at_the_loaded_address()
{
    const PRINTS: &[(&str, &str)] = &[
        // Changed by the program before the stop: read from memory, not the file.
        ("COUNTER", STATICS[0].2),
        ("NAME", r#"&str = "ferris""#),
        ("TABLE", "[u32; 4] = [1, 2, 3, 4]"),
        ("TABLE[2]", "u32 = 3"),
        ("config::LIMIT", "u16 = 512"),
        ("ferros_fixture_values_statics::NAME", r#"&str = "ferris""#),
        ("::ferros_fixture_values_statics::config::LIMIT", "u16 = 512"),
        ("CALLS", "u8 = 7"),
    ];
    const FAILURES: &[(&str, &str)] = &[
        // Thread-locals have no fixed address; they are not statics.
        ("PER_THREAD", "no variable named 'PER_THREAD' in the selected frame"),
        ("other::NAME", "no variable named 'other::NAME' in the selected frame"),
        (
            "TABLE[4]",
            "cannot evaluate 'TABLE[4]': `TABLE[4]`: index 4 is out of bounds for length 4",
        ),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_statics");
        let break_at = format!("break values_statics.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = PRINTS
            .iter()
            .chain(FAILURES)
            .map(|(path, _)| format!("print {path}"))
            .collect();
        let mut commands = vec![break_at.as_str(), "continue", "globals values_statics", "globals config"];
        commands.extend(prints.iter().map(String::as_str));
        commands.extend(["globals no_such_static", "quit"]);
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        let context = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);

        let globals = command_output(&run.stdout, "globals values_statics");
        let listed: Vec<_> = globals.lines().map(|line| line.rsplit_once(" @ ")).collect();
        let expected: Vec<_> = STATICS
            .iter()
            .map(|(name, ty, value)| {
                // Built in pieces: one `format!` with every argument trips `large_stack_arrays`.
                let mut line = format!("{PREFIX}{name}");
                write!(line, ": {ty} = {value}").expect("format a static");
                line
            })
            .collect();
        assert_eq!(
            listed.iter().map(|line| line.map(|(value, _)| value)).collect::<Vec<_>>(),
            expected.iter().map(|line| Some(line.as_str())).collect::<Vec<_>>(),
            "{context}"
        );
        // The listed address is where the running program has the static, slide included.
        let printed = run
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("COUNTER at "))
            .map(hex)
            .expect("the fixture prints COUNTER's address");
        assert_eq!(listed[0].map(|(_, address)| hex(address)), Some(printed), "{context}");
        assert_eq!(
            command_output(&run.stdout, "globals config"),
            format!("{PREFIX}config::LIMIT: u16 = 512 @ {}\n", listed[4].expect("LIMIT").1),
            "{context}"
        );
        assert_eq!(
            command_output(&run.stdout, "globals no_such_static"),
            "No statics matching 'no_such_static'.\n"
        );

        for &(path, expected) in PRINTS {
            let rendered = command_output(&run.stdout, &format!("print {path}"));
            let expected = if path == "COUNTER" {
                format!("{path}: {COUNTER_TYPE} = {expected}\n")
            } else {
                format!("{path}: {expected}\n")
            };
            assert_eq!(rendered, expected, "{path}:\n{context}");
        }
        let errors: Vec<_> = run.stderr.lines().filter_map(|line| line.strip_prefix("error: ")).collect();
        let expected: Vec<_> = FAILURES.iter().map(|(_, message)| *message).collect();
        assert_eq!(errors, expected, "{context}");
    });
}

#[test]
fn statics_follow_the_image_slide()
{
    harness::run(|t| {
        let fixture = fixture!("values_statics");
        let mut d = t.launch(&fixture);
        d.set_breakpoint(BreakpointLocation::Line {
            file: "values_statics.rs".to_owned(),
            line: fixture.marker_line("inspect"),
        })
        .expect("break at inspect");
        d.resume().expect("resume");
        let stop = d.wait(Some(Duration::from_secs(5))).expect("wait");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");

        let statics = d.statics(Some(PREFIX)).expect("statics");
        let names: Vec<_> = statics.iter().map(|found| found.name.as_str()).collect();
        let expected: Vec<_> = STATICS.iter().map(|(name, ..)| format!("{PREFIX}{name}")).collect();
        assert_eq!(names, expected);
        let images = d.images().expect("images");
        let main = images
            .iter()
            .find(|image| image.path == fixture.path())
            .expect("the main executable image")
            .clone();
        for found in &statics {
            let mapped = d.runtime_address(&main, found.file_address).expect("image mapping");
            assert_eq!(found.address, mapped, "{found:?}");
            assert_ne!(
                found.address.map(Address::value),
                Some(found.file_address),
                "the executable is slid, so a static's address differs from its link-time one: {found:?}"
            );
        }
        let counter = d
            .static_variable("COUNTER")
            .expect("look up COUNTER")
            .expect("COUNTER is a static");
        assert_eq!(counter.name, format!("{PREFIX}COUNTER"));
        assert_eq!(counter.value.bytes.as_deref(), Ok(&42_u64.to_le_bytes()[..]));
        assert!(d.static_variable("PER_THREAD").expect("look up PER_THREAD").is_none());
        // A name matches whole `::` segments only.
        assert!(d.static_variable("OUNTER").expect("look up OUNTER").is_none());
    });
}

fn command_output(stdout: &str, command: &str) -> String
{
    let prefix = format!("{command}\n");
    stdout
        .split("(ferros) ")
        .find_map(|section| section.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no output for {command}:\n{stdout}"))
        .to_owned()
}

fn hex(text: &str) -> u64
{
    let digits = text.trim().strip_prefix("0x").unwrap_or_else(|| panic!("not hex: {text}"));
    u64::from_str_radix(digits, 16).unwrap_or_else(|error| panic!("{text}: {error}"))
}
