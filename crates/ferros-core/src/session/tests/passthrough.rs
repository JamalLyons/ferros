use super::*;

#[test]
fn register_and_memory_pass_through()
{
    let mock = process();
    let mut session = launched(&mock);
    let mut registers = session.read_registers().expect("read");
    registers.general[0] = 7;
    session.write_registers(&registers).expect("write");
    assert_eq!(mock.peek_registers(T1).expect("t1").general[0], 7);
    session.write_registers_for(T2, &registers).expect("write t2");
    assert_eq!(session.read_registers_for(T2).expect("read t2").general[0], 7);
    assert!(matches!(
        session.read_registers_for(ThreadId(9)),
        Err(DebuggerError::Backend(BackendError::NotFound(_)))
    ));

    session.write_memory(Address::new(CODE), &[9, 9]).expect("write");
    assert_eq!(session.read_memory(Address::new(CODE), 3).expect("read"), vec![9, 9, 2]);
    assert_eq!(session.memory_regions().expect("regions").len(), 1);
    assert!(session.images().expect("images").is_empty());
}

#[test]
fn process_without_threads_has_no_selection()
{
    let mock = MockBackend::new().with_region(Address::new(CODE), "r-x", vec![0; 8]);
    let mut session = launched(&mock);
    assert_eq!(session.selected_thread(), None);
    assert!(matches!(session.read_registers(), Err(DebuggerError::NoThreadSelected)));
    assert!(matches!(session.stack_trace(4), Err(DebuggerError::NoThreadSelected)));
}

#[test]
fn backend_mut_bypasses_the_table()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    session
        .backend_mut()
        .write_memory(Address::new(BP), &original(BP))
        .expect("raw write");
    // The table still believes the trap is inserted.
    assert!(session.breakpoint_info(id).expect("info").enabled);
    assert_eq!(session.backend().architecture(), Architecture::Arm64);
}

/// Reads through the session show the original bytes, not traps.
#[test]
fn read_memory_hides_trap_bytes()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    assert_eq!(session.read_memory(Address::new(BP), 4).expect("read"), original(BP));
}
