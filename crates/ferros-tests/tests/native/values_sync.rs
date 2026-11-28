//! Render locks, atomics, lazily set cells, weak references and pins from a stopped Rust
//! program (issues #409, #422).
#![cfg(target_os = "macos")]

use ferros_tests::{fixture, harness, transcript};

#[test]
fn print_shows_synchronized_values_and_their_state()
{
    const SAMPLES: &[(&str, &str)] = &[
        ("mutex", "Mutex(unlocked) 9"),
        ("released", "Mutex(unlocked) 10"),
        // Held by another thread: read from the system mutex, without calling into the target.
        ("held", "Arc(strong: 2, weak: 0) Mutex(locked) 14"),
        ("poisoned", "Arc(strong: 1, weak: 0) Mutex(poisoned) 13"),
        ("rwlock", "RwLock(unlocked) 8"),
        ("read_locked", "RwLock(readers: 1) 11"),
        ("write_locked", "RwLock(write-locked) 15"),
        // A guard shows the value it holds (issue #422).
        ("guarded", "Mutex(locked) 16"),
        ("guard", "MutexGuard 16"),
        ("reader", "RwLockReadGuard 11"),
        ("writer", "RwLockWriteGuard 15"),
        ("poisoned_guard", "MutexGuard(poisoned) 17"),
        ("poisoned_writer", "RwLockWriteGuard 19"),
        ("atomic_u32", "7"),
        ("atomic_bool", "true"),
        ("null_ptr", "0x0"),
        ("once_set", "OnceCell(4)"),
        ("once_unset", "OnceCell(<uninit>)"),
        ("once_lock", "OnceLock(6)"),
        ("once_lock_unset", "OnceLock(<uninit>)"),
        ("unsafe_cell", "3"),
        ("rc_weak", "Weak(strong: 1, weak: 1) 20"),
        ("arc_weak", "Weak(strong: 1, weak: 1) 21"),
        ("arc_dangling", "Weak(dangling)"),
        ("rc_new", "Weak(dangling)"),
        ("pinned", "5"),
        // Unsized OS and C string payloads show quoted text.
        ("rc_path", r#"Rc(strong: 1, weak: 0) "/tmp""#),
        ("arc_os_str", r#"Arc(strong: 1, weak: 0) "os""#),
        ("rc_c_str", r#"Rc(strong: 1, weak: 0) c"cs""#),
    ];
    let _ = transcript::ferros_bin();
    harness::run(|t| {
        let fixture = fixture!("values_sync");
        let break_at = format!("break values_sync.rs:{}", fixture.marker_line("inspect"));
        let names = SAMPLES.iter().map(|(name, _)| *name).chain(["atomic_ptr", "pinned_ref"]);
        let prints: Vec<_> = names.map(|name| format!("print {name}")).collect();
        let mut commands = vec![break_at.as_str(), "continue"];
        commands.extend(prints.iter().map(String::as_str));
        commands.push("quit");
        let run = transcript::launch_batch(&t.tag(), &fixture, &[], &commands);
        assert!(run.status.success(), "status {:?}, stderr: {}", run.status, run.stderr);
        let rendered = |name: &str| {
            let prefix = format!("{name}: ");
            run.stdout
                .lines()
                .find_map(|line| line.strip_prefix(&prefix)?.split_once(" = ").map(|(_, value)| value))
        };
        for &(name, expected) in SAMPLES {
            assert_eq!(rendered(name), Some(expected), "{name}:\n{}", run.stdout);
        }
        // `atomic_ptr` and `pinned_ref` both point at the same local.
        let pointer = rendered("atomic_ptr").expect("atomic_ptr");
        assert!(pointer.starts_with("0x") && pointer != "0x0", "atomic_ptr: {pointer}");
        assert_eq!(
            rendered("pinned_ref"),
            Some(format!("{pointer} -> 12").as_str()),
            "{}",
            run.stdout
        );
    });
}

/// Without a platform runtime the session cannot read a system mutex, so a `Mutex` that
/// has been locked shows its value and no lock state; one never locked still shows
/// `unlocked`, and poisoning still shows.
#[test]
fn without_a_runtime_an_allocated_system_mutex_shows_no_lock_state()
{
    use std::time::Duration;

    use ferros_core::backend::StdioSpec;
    use ferros_core::values::format::FormatOptions;
    use ferros_core::{BreakpointLocation, LaunchSpec, Session, StopEvent};
    use ferros_macos::{MacOSBackend, MachOFormat};

    harness::run(|_| {
        let fixture = fixture!("values_sync");
        let mut session = Session::new(Box::new(MacOSBackend::new().expect("backend")), MachOFormat::shared());
        let spec = LaunchSpec::new(fixture.path())
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        session.launch(&spec).expect("launch fixture");
        session
            .set_breakpoint(BreakpointLocation::Line {
                file: "values_sync.rs".to_owned(),
                line: fixture.marker_line("inspect"),
            })
            .expect("break at inspect");
        session.resume().expect("resume");
        let stop = session.wait(Some(Duration::from_secs(10))).expect("wait");
        assert!(matches!(stop, Some(StopEvent::Breakpoint { .. })), "got {stop:?}");
        let variables = session.selected_frame_variables().expect("locals");
        let mut rendered = |name: &str| {
            let variable = variables.iter().find(|variable| variable.name == name).expect(name);
            session.format_variable(variable, FormatOptions::default()).1
        };
        assert_eq!(rendered("mutex"), "Mutex(unlocked) 9");
        assert_eq!(rendered("released"), "Mutex 10");
        assert_eq!(rendered("held"), "Arc(strong: 2, weak: 0) Mutex 14");
        assert_eq!(rendered("poisoned"), "Arc(strong: 1, weak: 0) Mutex(poisoned) 13");
        session.kill().expect("kill fixture");
    });
}
