use super::*;

#[test]
fn breakpoint_insert_remove_disable_enable_restore_bytes()
{
    let mock = process();
    let mut session = launched(&mock);

    let id = session.add_breakpoint(software(BP)).expect("add");
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
    let info = session.breakpoint_info(id).expect("info");
    assert!(info.enabled);
    assert_eq!(info.state, BreakpointState::Resolved);
    assert_eq!(info.address, Address::new(BP));
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::ReadMemory {
                address: Address::new(BP),
                len: 4
            },
            MockCall::WriteMemory {
                address: Address::new(BP),
                data: TRAP.to_vec()
            },
        ]
    );

    session.disable_breakpoint(id).expect("disable");
    assert_eq!(peek(&mock, BP), original(BP));
    let info = session.breakpoint_info(id).expect("info");
    assert!(!info.enabled);
    assert_eq!(info.state, BreakpointState::Disabled);
    // Disabling twice does nothing.
    let _ = mock.take_calls();
    session.disable_breakpoint(id).expect("disable again");
    assert!(mock.calls().is_empty());

    session.enable_breakpoint(id).expect("enable");
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
    // Enabling writes the trap without reading memory again.
    assert_eq!(
        mock.take_calls(),
        [MockCall::WriteMemory {
            address: Address::new(BP),
            data: TRAP.to_vec()
        }]
    );
    assert!(session.breakpoint_info(id).expect("info").enabled);

    session.remove_breakpoint(id).expect("remove");
    assert_eq!(peek(&mock, BP), original(BP));
    assert!(session.breakpoints().is_empty());
    assert!(matches!(
        session.breakpoint_info(id),
        Err(DebuggerError::BreakpointIdNotFound(_))
    ));
    assert!(matches!(
        session.remove_breakpoint(id),
        Err(DebuggerError::BreakpointIdNotFound(_))
    ));

    // Removing a disabled breakpoint does not write memory.
    let id = session.add_breakpoint(software(BP)).expect("add");
    session.disable_breakpoint(id).expect("disable");
    let _ = mock.take_calls();
    session.remove_breakpoint(id).expect("remove disabled");
    assert!(mock.calls().is_empty());
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn failed_trap_write_leaves_table_unchanged()
{
    let mock = process();
    let mut session = launched(&mock);
    mock.fail_next(
        MockCallKind::WriteMemory,
        BackendError::Os {
            code: 2,
            message: "protection".into(),
        },
    );
    let result = session.add_breakpoint(software(BP));
    assert!(matches!(
        result,
        Err(DebuggerError::Backend(BackendError::Os { code: 2, .. }))
    ));
    assert!(session.breakpoints().is_empty());
    assert_eq!(peek(&mock, BP), original(BP));

    // A failed read leaves it unchanged too.
    mock.fail_next(MockCallKind::ReadMemory, BackendError::NotFound("unmapped".into()));
    assert!(matches!(
        session.add_breakpoint(software(BP)),
        Err(DebuggerError::Backend(BackendError::NotFound(_)))
    ));
    assert!(session.breakpoints().is_empty());

    // A failed restore keeps the breakpoint enabled; a failed re-insert keeps it disabled.
    let id = session.add_breakpoint(software(BP)).expect("add");
    let os = || BackendError::Os {
        code: 3,
        message: "write".into(),
    };
    mock.fail_next(MockCallKind::WriteMemory, os());
    assert!(session.disable_breakpoint(id).is_err());
    assert!(session.breakpoint_info(id).expect("info").enabled);
    mock.fail_next(MockCallKind::WriteMemory, os());
    assert!(session.remove_breakpoint(id).is_err());
    assert!(session.breakpoint_info(id).is_ok());
    session.disable_breakpoint(id).expect("disable");
    mock.fail_next(MockCallKind::WriteMemory, os());
    assert!(session.enable_breakpoint(id).is_err());
    assert!(!session.breakpoint_info(id).expect("info").enabled);
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn duplicate_software_breakpoint_returns_same_id()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();
    assert_eq!(session.add_breakpoint(software(BP)).expect("again"), id);
    assert!(mock.calls().is_empty());
    assert_eq!(session.breakpoints().len(), 1);
    // The saved original bytes are still the real instruction, not the trap.
    session.remove_breakpoint(id).expect("remove");
    assert_eq!(peek(&mock, BP), original(BP));
}

#[test]
fn hardware_breakpoints_and_watchpoints_are_unsupported()
{
    let mock = process();
    let mut session = launched(&mock);
    let address = Address::new(BP);
    assert!(is_unsupported(
        &session.add_breakpoint(BreakpointRequest::Hardware { address })
    ));
    assert!(is_unsupported(&session.add_breakpoint(BreakpointRequest::Watchpoint {
        address,
        length: 8,
        access: WatchpointAccess::Write,
    })));
    assert!(mock.calls().is_empty());
    assert!(session.breakpoints().is_empty());
}

#[test]
fn unsupported_architecture_refuses_software_breakpoints()
{
    let mock = process().with_architecture(Architecture::X86_64);
    let mut session = launched(&mock);
    assert!(is_unsupported(&session.add_breakpoint(software(BP))));
    assert!(mock.calls().is_empty());
}

#[test]
fn overlapping_breakpoint_is_refused_before_any_backend_call()
{
    let mock = process();
    let mut session = launched(&mock);
    session.add_breakpoint(software(BP)).expect("add");
    let _ = mock.take_calls();

    for address in [BP + 1, BP + 3, BP - 1, BP - 3] {
        assert!(
            matches!(
                session.add_breakpoint(software(address)),
                Err(DebuggerError::InvalidArgument(_))
            ),
            "a trap at 0x{address:x} overlaps the one at 0x{BP:x}"
        );
    }
    assert!(mock.take_calls().is_empty(), "an overlapping breakpoint reached the backend");
    assert_eq!(session.breakpoints().len(), 1);

    // The neighbouring instruction does not overlap.
    session.add_breakpoint(software(BP + 4)).expect("add at the next instruction");
    drop(session);
    assert_eq!(
        mock.peek_memory(Address::new(BP), 8).expect("peek"),
        [original(BP), original(BP + 4)].concat()
    );
}

fn assert_install_refused(mock: &MockBackend, session: &mut Session, builder: BreakpointBuilder, option: &str)
{
    let result = builder.install(session);
    assert!(
        matches!(&result, Err(DebuggerError::InvalidArgument(message)) if message.contains(option)),
        "{option}: {result:?}"
    );
    assert!(session.breakpoints().is_empty(), "{option}: table not empty");
    assert!(
        mock.calls().iter().all(|call| !matches!(call, MockCall::WriteMemory { .. })),
        "{option}: trap written: {:?}",
        mock.calls()
    );
    assert_eq!(peek(mock, BP), original(BP), "{option}: memory changed");
}

#[test]
fn install_rejects_a_hit_count_instead_of_dropping_it()
{
    let at = || BreakpointBuilder::software(Address::new(BP));

    let mock = process();
    let mut session = launched(&mock);
    assert_install_refused(&mock, &mut session, at().with_hit_count(5), "hit count");

    let mock = process();
    let mut session = launched(&mock);
    assert_install_refused(&mock, &mut session, at().with_condition(|_| true), "condition");

    let mock = process();
    let mut session = launched(&mock);
    assert_install_refused(&mock, &mut session, at().for_thread(T1), "thread filter");

    let mock = process();
    let mut session = launched(&mock);
    assert_install_refused(&mock, &mut session, at().with_command("continue"), "commands");
}

#[test]
fn install_without_options_plants_the_trap()
{
    let mock = process();
    let mut session = launched(&mock);
    let id = BreakpointBuilder::software(Address::new(BP))
        .install(&mut session)
        .expect("install");
    assert_eq!(peek(&mock, BP), TRAP.to_vec());
    assert_eq!(session.breakpoint_info(id).expect("info").address, Address::new(BP));
}
