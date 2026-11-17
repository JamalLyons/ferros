//! Native process-group placement (issue #94).
#![cfg(target_os = "macos")]

use ferros_core::backend::{LaunchSpec, ProcessGroup, StdioSpec};
use ferros_core::{ProcessId, Session};
use ferros_macos::launch::process_group;
use ferros_tests::{fixture, harness};

#[test]
fn new_group_is_distinct_and_default_inherits()
{
    harness::run(|t| {
        let tag = t.tag();
        let fixture = fixture!("hello");
        let parent = process_group(ProcessId(std::process::id())).expect("test process group");
        for new_group in [false, true] {
            let mut session = Session::new(Box::new(ferros_macos::MacOSBackend::new().expect("backend")));
            let mut spec = LaunchSpec::new(fixture.path())
                .arg(tag.as_str())
                .stdin(StdioSpec::Null)
                .stdout(StdioSpec::Null)
                .stderr(StdioSpec::Null);
            if new_group {
                spec = spec.process_group(ProcessGroup::New);
            }
            let pid = session.launch(&spec).expect("launch");
            let group = process_group(pid).expect("debuggee process group");
            if new_group {
                assert_eq!(group, pid, "a new group is led by the launched child");
                assert_ne!(group, parent, "child must not receive the debugger group's signals");
            } else {
                assert_eq!(group, parent, "default inherits the parent's group");
            }
            session.kill().expect("kill and reap");
        }
    });
}
