use super::*;
use crate::session::{BreakpointLocation, LogicalBreakpointId};

fn at(address: u64) -> BreakpointLocation
{
    BreakpointLocation::Address(Address::new(address))
}

/// Two logical breakpoints at one address share its trap: it stays while either is
/// enabled, and is removed only with the last one.
#[test]
fn shared_site_keeps_its_trap_until_the_last_owner_goes()
{
    let mock = process();
    let mut session = launched(&mock);
    let first = session.set_breakpoint(at(BP)).expect("first");
    let second = session.set_breakpoint(at(BP)).expect("second");
    assert_ne!(first, second);
    assert_eq!(peek(&mock, BP), TRAP);

    session.disable_logical_breakpoint(first).expect("disable first");
    assert_eq!(peek(&mock, BP), TRAP, "the second still needs the trap");
    session.delete_breakpoint(second).expect("delete second");
    assert_eq!(peek(&mock, BP), original(BP), "no enabled owner is left");
    assert_eq!(session.breakpoints().len(), 1, "the disabled first keeps its entry");

    session.enable_logical_breakpoint(first).expect("enable first");
    assert_eq!(peek(&mock, BP), TRAP);
    session.delete_breakpoint(first).expect("delete first");
    assert_eq!(peek(&mock, BP), original(BP));
    assert!(session.breakpoints().is_empty());
    assert!(session.logical_breakpoints().is_empty());
}

/// Deleting two disabled breakpoints that share an address leaves no trap entry.
#[test]
fn deleting_shared_disabled_breakpoints_leaves_no_entry()
{
    let mock = process();
    let mut session = launched(&mock);
    let first = session.set_breakpoint(at(BP)).expect("first");
    let second = session.set_breakpoint(at(BP)).expect("second");
    session.disable_logical_breakpoint(first).expect("disable first");
    session.disable_logical_breakpoint(second).expect("disable second");
    assert_eq!(peek(&mock, BP), original(BP));
    session.delete_breakpoint(first).expect("delete first");
    assert_eq!(session.breakpoints().len(), 1, "the second still has its site");
    session.delete_breakpoint(second).expect("delete second");
    assert_eq!(session.breakpoints().len(), 0);
    assert_eq!(peek(&mock, BP), original(BP));
}

/// A hit counts on the logical breakpoint, and `breakpoint_hit` names it.
#[test]
fn hit_is_reported_on_the_logical_breakpoint()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.set_breakpoint(at(BP)).expect("set");
    mock.script(MockStop::new(StopEvent::Breakpoint {
        thread: T1,
        pc: Address::new(BP),
    }));
    session.resume().expect("resume");
    session.wait(Some(Duration::from_secs(1))).expect("wait").expect("stop");
    let hit = session.breakpoint_hit().expect("a hit");
    assert_eq!(hit.id, id);
    assert_eq!(hit.thread, T1);
    assert_eq!(hit.site.address, Address::new(BP));
    assert_eq!(session.logical_breakpoints()[0].hit_count, 1);
}

/// Address breakpoints are deleted when the process ends; unknown ids are errors.
#[test]
fn address_breakpoints_end_with_the_process()
{
    let mock = process();
    let mut session = launched(&mock);
    session.set_breakpoint(at(BP)).expect("set");
    session.kill().expect("kill");
    assert!(session.logical_breakpoints().is_empty());
    assert!(matches!(
        session.delete_breakpoint(LogicalBreakpointId::from_raw(9)),
        Err(DebuggerError::BreakpointIdNotFound(9))
    ));
}

/// A failed trap write adds no logical breakpoint and leaves memory unchanged.
#[test]
fn failed_insert_adds_nothing()
{
    let mock = process();
    let mut session = launched(&mock);
    // Outside every mapped region: reading the original bytes fails.
    assert!(session.set_breakpoint(at(0x9000)).is_err());
    assert!(session.logical_breakpoints().is_empty());
    assert!(session.breakpoints().is_empty());
}
