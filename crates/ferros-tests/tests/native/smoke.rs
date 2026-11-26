//! Smoke tests: the portable `ferros_core::Session` engine, driving the macOS backend,
//! against a real fixture process.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::SessionState;
use ferros_tests::{fixture, harness};

#[test]
fn launch_hello_stops_at_entry()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        assert!(d.pid().0 > 1, "unexpected pid {}", d.pid().0);
        assert_eq!(d.state(), SessionState::Stopped, "process should be stopped at entry");
        assert_eq!(d.session().pid(), Some(d.pid()));
        assert!(
            d.last_stop().is_none(),
            "the initial stop has no event, got {:?}",
            d.last_stop()
        );
    });
}

#[test]
fn read_registers_pc_in_executable_region()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let regs = d.read_registers().expect("read_registers");
        let regions = d.memory_regions().expect("memory_regions");
        let region = regions
            .iter()
            .find(|region| region.contains(regs.pc))
            .unwrap_or_else(|| panic!("pc {} is not inside any memory region", regs.pc));
        assert!(
            region.is_executable(),
            "pc {} lies in a non-executable region: {region:?}",
            regs.pc
        );
    });
}

#[test]
fn read_memory_at_pc()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let regs = d.read_registers().expect("read_registers");
        let bytes = d.read_memory(regs.pc, 4).expect("read_memory");
        assert_eq!(bytes.len(), 4, "read_memory(pc, 4) returned {bytes:?}");
    });
}

#[test]
fn thread_list_contains_selected_thread()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        let threads = d.threads().expect("threads");
        assert!(!threads.is_empty(), "thread list is empty");
        let selected = d.selected_thread().expect("no selected thread");
        assert!(threads.contains(&selected), "selected thread {selected:?} not in {threads:?}");
    });
}

#[test]
fn resume_runs_the_process()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        d.resume().expect("resume");
        assert_eq!(d.state(), SessionState::Running);
        assert!(
            d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)),
            "fixture output never arrived; stdout: {:?}, stderr: {:?}",
            d.stdout(),
            d.stderr()
        );
    });
}

#[test]
fn detach_releases_the_process()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        d.detach().expect("detach");
        assert_eq!(d.state(), SessionState::NotStarted, "state after detach");
        assert_eq!(d.session().pid(), None);
        // The detached process is no longer suspended: it runs to completion on its own.
        assert!(
            d.wait_for_stdout("hello from ferros fixture", Duration::from_secs(10)),
            "detached fixture never printed; stdout: {:?}, stderr: {:?}",
            d.stdout(),
            d.stderr()
        );
    });
}
