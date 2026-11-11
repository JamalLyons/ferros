//! Smoke tests: the existing `ferros_core::Debugger` trait against a real fixture process.
#![cfg(target_os = "macos")]

use std::time::Duration;

use ferros_core::StopReason;
use ferros_tests::{fixture, harness};

#[test]
fn launch_hello_stops_at_entry()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        assert!(d.pid().0 > 1, "unexpected pid {}", d.pid().0);
        assert!(d.is_attached(), "debugger should be attached after launch");
        assert!(d.is_stopped(), "process should be stopped at entry");
        assert_eq!(d.stop_reason(), StopReason::Suspended);
    });
}

#[test]
fn read_registers_pc_in_executable_region()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        let regs = d.read_registers().expect("read_registers");
        let regions = d.get_memory_regions().expect("get_memory_regions");
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
        let d = t.launch(&fixture!("hello"));
        let regs = d.read_registers().expect("read_registers");
        let bytes = d.read_memory(regs.pc, 4).expect("read_memory");
        assert_eq!(bytes.len(), 4, "read_memory(pc, 4) returned {bytes:?}");
    });
}

#[test]
fn thread_list_contains_active_thread()
{
    harness::run(|t| {
        let d = t.launch(&fixture!("hello"));
        let threads = d.threads().expect("threads");
        assert!(!threads.is_empty(), "thread list is empty");
        let active = d.active_thread().expect("no active thread");
        assert!(threads.contains(&active), "active thread {active:?} not in {threads:?}");
    });
}

#[test]
fn resume_runs_the_process()
{
    harness::run(|t| {
        let mut d = t.launch(&fixture!("hello"));
        d.resume().expect("resume");
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
        assert!(!d.is_attached(), "is_attached() still true after detach");
    });
}
