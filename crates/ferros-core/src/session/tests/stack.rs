use super::*;

#[test]
fn stack_trace_without_images_returns_the_current_frame()
{
    let mock = process();
    let mut session = launched(&mock);
    let frames = session.stack_trace(16).expect("stack trace");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].pc, Address::new(CODE));
    assert_eq!(frames[0].thread, T1);
    assert!(frames[0].symbol.is_none());
    assert_eq!(mock.take_calls(), [MockCall::ReadRegisters(T1), MockCall::Images]);

    assert!(session.stack_trace(0).expect("zero frames").is_empty());
    assert_eq!(
        session
            .symbolicate(Address::new(CODE))
            .expect("symbolicate")
            .map(|s| s.frames.len()),
        None
    );
}

#[test]
fn stack_trace_reads_memory_through_the_backend_and_skips_bad_images()
{
    // Thread 1 with a frame pointer at 0x2000: saved fp 0, saved lr 0x1010.
    let mut registers = regs_at(CODE);
    registers.fp = Address::new(0x2000);
    registers.sp = Address::new(0x2000);
    let mut stack = vec![0u8; 16];
    stack[8..].copy_from_slice(&0x1010u64.to_le_bytes());
    let mock = MockBackend::new()
        .with_thread(T1, registers)
        .with_region(Address::new(CODE), "r-x", vec![0; 32])
        .with_region(Address::new(0x2000), "rw-", stack)
        .with_image(LoadedImage {
            path: "/nonexistent/ferros-session-test-image".into(),
            load_address: Address::new(CODE),
            build_id: crate::types::BuildIdentity::Unknown,
            role: crate::backend::ImageRole::MainExecutable,
        });
    let mut session = launched(&mock);
    let frames = session.stack_trace(8).expect("stack trace");
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(pcs[..2], [CODE, 0x1010], "{frames:?}");
    assert!(mock.calls().contains(&MockCall::ReadMemory {
        address: Address::new(0x2000),
        len: 8
    }));

    // Symbolication still works after an image failed to load: the backend is asked for
    // its image list and nothing else.
    let _ = mock.take_calls();
    session.symbolicate(Address::new(CODE)).expect("symbolicate");
    assert_eq!(mock.take_calls(), [MockCall::Images]);
}

#[test]
fn runtime_address_reports_the_original_header_failure_once_read()
{
    // Issue #78: an image whose mapped header cannot be parsed is an error naming the
    // cause, not `Ok(None)`, and the result is kept rather than read again.
    let image = LoadedImage {
        path: "/nonexistent/ferros-session-test-image".into(),
        load_address: Address::new(CODE),
        build_id: crate::types::BuildIdentity::Unknown,
        role: crate::backend::ImageRole::MainExecutable,
    };
    let mock = process().with_image(image.clone());
    let mut session = launched(&mock);
    let _ = mock.take_calls();
    for _ in 0..2 {
        let error = session
            .runtime_address(&image, 0x1_0000_0000)
            .expect_err("unparseable header");
        let DebuggerError::ImageMapping { path, detail } = &error else {
            panic!("unexpected error {error}");
        };
        assert_eq!(path, &image.path);
        assert!(detail.contains("mock header at"), "{detail}");
    }
    assert_eq!(
        mock.take_calls(),
        [
            MockCall::Images,
            MockCall::ReadMemory {
                address: Address::new(CODE),
                len: MockObjectFormat::HEADER_SIZE
            },
            MockCall::Images
        ]
    );
    let moved = LoadedImage {
        load_address: Address::new(CODE + 0x1000),
        ..image
    };
    assert_eq!(session.runtime_address(&moved, 0x1_0000_0000).expect("not loaded"), None);
}

#[test]
fn symbolicate_reports_an_unreadable_image_every_time_and_stack_traces_continue()
{
    // Issue #85: a mapped image whose file cannot be read is an error carrying the
    // original failure on every request, not a silent `Ok(None)` cached after the first.
    const IMAGE: u64 = 0x10_0000;
    let image = LoadedImage {
        path: "/nonexistent/ferros-session-test-image".into(),
        load_address: Address::new(IMAGE),
        build_id: crate::types::BuildIdentity::Unknown,
        role: crate::backend::ImageRole::MainExecutable,
    };
    let mock = MockBackend::new()
        .with_thread(T1, regs_at(IMAGE + 0x40))
        .with_region(Address::new(IMAGE), "r-x", mapped_text_header())
        .with_image(image.clone());
    let mut session = launched(&mock);
    for _ in 0..2 {
        let error = session.symbolicate(Address::new(IMAGE + 0x40)).expect_err("unreadable image");
        let DebuggerError::SymbolLoad { path, source } = &error else {
            panic!("unexpected error {error}");
        };
        assert_eq!(path, &image.path);
        assert!(
            matches!(&**source, DebuggerError::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
            "{source}"
        );
        assert!(error.to_string().contains("ferros-session-test-image"), "{error}");
    }
    // Stack traces still return raw frames.
    let frames = session.stack_trace(4).expect("stack trace");
    assert_eq!(frames[0].pc, Address::new(IMAGE + 0x40));
    assert!(frames[0].symbol.is_none());
}

#[test]
fn stack_trace_ignores_unsupported_image_listing()
{
    let mock = process();
    mock.fail_next(MockCallKind::Images, BackendError::Unsupported("no images".into()));
    let mut session = launched(&mock);
    assert_eq!(session.stack_trace(4).expect("stack trace").len(), 1);

    mock.fail_next(
        MockCallKind::Images,
        BackendError::Os {
            code: 4,
            message: "image list".into(),
        },
    );
    assert!(matches!(
        session.stack_trace(4),
        Err(DebuggerError::Backend(BackendError::Os { code: 4, .. }))
    ));
}
