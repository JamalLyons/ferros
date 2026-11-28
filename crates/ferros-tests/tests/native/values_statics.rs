//! Statics and globals: `print` of a static by name and `globals [filter]` (issue #392),
//! and statics inside impl methods, named after their impl (issue #424).
#![cfg(target_os = "macos")]

use std::fmt::Write as _;
use std::time::Duration;

use ferros_core::{Address, BreakpointLocation, StopEvent};
use ferros_tests::{fixture, harness, transcript};

const PREFIX: &str = "ferros_fixture_values_statics::";
const COUNTER_TYPE: &str = "core::sync::atomic::Atomic<u64>";
// The atomic printer (#409) shows the stored value.
const COUNTER_VALUE: &str = "42";

/// The `static X` in each impl's `fn f`, by the name `globals` lists and `print` accepts,
/// in `globals` order, with its value.
const IMPL_STATICS: &[(&str, &str)] = &[
    ("<alloc::vec::Vec<_> as ferros_fixture_values_statics::Unit>::f::X", "400"),
    ("<ferros_fixture_values_statics::Feet>::f::X", "200"),
    (
        "<ferros_fixture_values_statics::Meters as ferros_fixture_values_statics::Unit>::f::X",
        "300",
    ),
    ("<ferros_fixture_values_statics::Meters>::f::X", "100"),
];

/// The fixture's other statics in `globals` order (by name, then address), after
/// [`IMPL_STATICS`], with their types and values, named without [`PREFIX`]. The two
/// `blocks::S` are separate rows: the first block's has the lower address.
const STATICS: &[(&str, &str, &str)] = &[
    ("COUNTER", COUNTER_TYPE, COUNTER_VALUE),
    ("NAME", "&str", r#""ferris""#),
    ("TABLE", "[u32; 4]", "[1, 2, 3, 4]"),
    ("blocks::S", "u32", "11"),
    ("blocks::S", "u32", "22"),
    ("calls::CALLS", "u8", "7"),
    ("config::LIMIT", "u16", "512"),
    ("stack::depth", "i32", "-1"),
];

#[test]
fn print_and_globals_read_statics_at_the_loaded_address()
{
    /// `print` path, then the output after `<path>`: ` (static <qualified name>): ` for a
    /// static, `: ` for a local.
    const PRINTS: &[(&str, &str, &str)] = &[
        // Changed by the program before the stop: read from memory, not the file.
        ("COUNTER", "COUNTER", ""),
        ("NAME", "NAME", r#"&str = "ferris""#),
        ("TABLE", "TABLE", "[u32; 4] = [1, 2, 3, 4]"),
        ("TABLE[2]", "TABLE", "u32 = 3"),
        ("config::LIMIT", "config::LIMIT", "u16 = 512"),
        ("ferros_fixture_values_statics::NAME", "NAME", r#"&str = "ferris""#),
        ("::ferros_fixture_values_statics::config::LIMIT", "config::LIMIT", "u16 = 512"),
        ("CALLS", "calls::CALLS", "u8 = 7"),
        ("stack::depth", "stack::depth", "i32 = -1"),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_statics");
        let break_at = format!("break values_statics.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = PRINTS.iter().map(|(path, ..)| format!("print {path}")).collect();
        let failing = vec![
            "print PER_THREAD",
            "print other::NAME",
            "print TABLE[4]",
            "print S",
            "print blocks::S",
        ];
        let mut commands = vec![break_at.as_str(), "continue", "globals values_statics", "globals config"];
        commands.extend(prints.iter().map(String::as_str));
        commands.extend(["print depth"]);
        commands.extend(failing);
        commands.extend(["globals no_such_static", "quit"]);
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        let context = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);

        let globals = command_output(&run.stdout, "globals values_statics");
        let listed: Vec<_> = globals.lines().map(|line| line.rsplit_once(" @ ")).collect();
        let expected: Vec<_> = IMPL_STATICS
            .iter()
            .map(|(name, value)| format!("{name}: u32 = {value}"))
            .chain(STATICS.iter().map(|(name, ty, value)| {
                // Built in pieces: one `format!` with every argument trips `large_stack_arrays`.
                let mut line = format!("{PREFIX}{name}");
                write!(line, ": {ty} = {value}").expect("format a static");
                line
            }))
            .collect();
        assert_eq!(
            listed.iter().map(|line| line.map(|(value, _)| value)).collect::<Vec<_>>(),
            expected.iter().map(|line| Some(line.as_str())).collect::<Vec<_>>(),
            "{context}"
        );
        // Rows of `STATICS`, which follow `IMPL_STATICS`.
        let address = |row: usize| listed[IMPL_STATICS.len() + row].expect("a listed static").1;
        // The listed address is where the running program has the static, slide included.
        let printed = run
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("COUNTER at "))
            .map(hex)
            .expect("the fixture prints COUNTER's address");
        assert_eq!(hex(address(0)), printed, "{context}");
        assert!(hex(address(3)) < hex(address(4)), "{context}");
        assert_eq!(
            command_output(&run.stdout, "globals config"),
            format!("{PREFIX}config::LIMIT: u16 = 512 @ {}\n", address(6)),
            "{context}"
        );
        assert_eq!(
            command_output(&run.stdout, "globals no_such_static"),
            "No statics matching 'no_such_static'.\n"
        );

        for &(path, name, value) in PRINTS {
            let value = if value.is_empty() {
                format!("{COUNTER_TYPE} = {COUNTER_VALUE}")
            } else {
                value.to_owned()
            };
            // Built in pieces: one `format!` with every argument trips `large_stack_arrays`.
            let mut expected = format!("{path} (static {PREFIX}{name})");
            writeln!(expected, ": {value}").expect("format a print");
            let rendered = command_output(&run.stdout, &format!("print {path}"));
            assert_eq!(rendered, expected, "{path}:\n{context}");
        }
        // The local shadows `stack::depth`, as the help text promises.
        assert_eq!(command_output(&run.stdout, "print depth"), "depth: i32 = 5\n", "{context}");

        let errors: Vec<_> = run.stderr.lines().filter_map(|line| line.strip_prefix("error: ")).collect();
        let ambiguous = |name: &str| {
            let mut message = format!(
                "Invalid argument: '{name}' is ambiguous: it names {PREFIX}blocks::S @ {}",
                address(3)
            );
            write!(message, ", {PREFIX}blocks::S @ {}", address(4)).expect("format the candidates");
            message
        };
        let expected = vec![
            // Thread-locals have no fixed address; they are not statics.
            "no variable or static named 'PER_THREAD' in the selected frame".to_owned(),
            "no variable or static named 'other::NAME' in the selected frame".to_owned(),
            "cannot evaluate 'TABLE[4]': `TABLE[4]`: index 4 is out of bounds for length 4".to_owned(),
            ambiguous("S"),
            ambiguous("blocks::S"),
        ];
        assert_eq!(errors, expected, "{context}");
    });
}

/// Each impl's `f::X` prints by the name `globals` lists, with its own value, and `f::X`
/// alone is ambiguous between them.
#[test]
fn impl_statics_are_named_after_their_impl()
{
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_statics");
        let break_at = format!("break values_statics.rs:{}", fixture.marker_line("inspect"));
        let prints: Vec<_> = IMPL_STATICS.iter().map(|(name, _)| format!("print {name}")).collect();
        let mut commands = vec![break_at.as_str(), "continue", "globals ::f::X"];
        commands.extend(prints.iter().map(String::as_str));
        commands.extend(["print f::X", "quit"]);
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        let context = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);

        let globals = command_output(&run.stdout, "globals ::f::X");
        let listed: Vec<_> = globals.lines().filter_map(|line| line.rsplit_once(" @ ")).collect();
        assert_eq!(
            listed.iter().map(|&(row, _)| row).collect::<Vec<_>>(),
            IMPL_STATICS
                .iter()
                .map(|(name, value)| format!("{name}: u32 = {value}"))
                .collect::<Vec<_>>(),
            "{context}"
        );
        for &(name, value) in IMPL_STATICS {
            let rendered = command_output(&run.stdout, &format!("print {name}"));
            assert_eq!(rendered, format!("{name} (static {name}): u32 = {value}\n"), "{context}");
        }
        let mut ambiguous = "error: Invalid argument: 'f::X' is ambiguous: it names ".to_owned();
        for (row, &(_, address)) in listed.iter().enumerate() {
            let separator = if row == 0 { "" } else { ", " };
            write!(ambiguous, "{separator}{} @ {address}", IMPL_STATICS[row].0).expect("format a candidate");
        }
        assert_eq!(run.stderr.lines().collect::<Vec<_>>(), [ambiguous.as_str()], "{context}");
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
        let names: Vec<_> = statics.iter().map(|found| found.variable.name.as_str()).collect();
        let expected: Vec<_> = IMPL_STATICS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .chain(STATICS.iter().map(|(name, ..)| format!("{PREFIX}{name}")))
            .collect();
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
        assert_eq!(counter.variable.name, format!("{PREFIX}COUNTER"));
        assert_eq!(counter.variable.value.bytes.as_deref(), Ok(&42_u64.to_le_bytes()[..]));
        assert_eq!(
            Some(counter.variable),
            statics.get(IMPL_STATICS.len()).map(|found| found.variable.clone())
        );
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
