//! Frames named through the injected object format and target runtime.

use ferros_core::backend::LaunchSpec;
use ferros_core::backend::mock::{MockBackend, arm64_registers};
use ferros_core::objfile::mock::{MockObjectFormat, MockObjectImage};
use ferros_core::runtime::ExitSource;
use ferros_core::runtime::mock::MockRuntime;
use ferros_core::types::Address;
use ferros_core::{Session, arch};

use crate::helpers::*;

/// #193: the session names a frame without DWARF through the object format it was given,
/// not through a parser of its own. The image is in the neutral `MockObjectFormat`, whose
/// bytes no Mach-O parser accepts, and its symbol `mock_entry` is named as written (the
/// format strips no prefix).
#[test]
fn a_frame_is_named_through_the_injected_object_format()
{
    use ferros_core::backend::LoadedImage;
    use ferros_core::types::SymbolSource;

    const BASE: u64 = 0x5_0000_0000;
    let symbols = MockObjectImage::new(BASE + 0x1000..BASE + 0x1100, BASE + 0x4000, BASE + 0x5000)
        .with_symbol(BASE + 0x1000, "mock_entry")
        .with_symbol(BASE + 0x1080, "_mock_next");
    let mut registers = arm64_registers();
    registers.pc = Address::new(BASE + 0x1090);
    let launched = |format: std::sync::Arc<dyn ferros_core::objfile::ObjectFormat>| {
        let mock = MockBackend::new()
            .with_thread(THREAD, registers.clone())
            .with_region(Address::new(BASE), "r-x", symbols.header_bytes())
            .with_region(Address::new(BASE + 0x1000), "r-x", vec![0; 0x100])
            .with_region(Address::new(BASE + 0x4000), "r--", symbols.table_bytes())
            .with_region(Address::new(BASE + 0x5000), "r--", symbols.name_bytes())
            .with_image(LoadedImage {
                path: "/nonexistent/ferros-mock-format-image".into(),
                load_address: Address::new(BASE),
                build_id: ferros_core::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::MainExecutable,
            });
        let mut session = Session::new(Box::new(mock), format);
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        session
    };
    let mut session = launched(MockObjectFormat::shared());
    let frames = session.stack_trace(1).expect("stack trace");
    let symbol = frames[0].symbol.as_ref().expect("a symbol table name");
    assert_eq!(symbol.display_name(), "_mock_next");
    assert_eq!(symbol.source(), SymbolSource::SymbolTable);

    // The same image in a session given the Mach-O format has no name: the format, not
    // the session, decides what the image's bytes mean.
    #[cfg(target_os = "macos")]
    {
        let mut session = launched(ferros_macos::MachOFormat::shared());
        let frames = session.stack_trace(1).expect("stack trace");
        assert_eq!(frames[0].symbol, None, "{frames:?}");
    }
}

/// #201: the non-local exit entries come from the session's runtime, not from the engine.
/// A step out with `exit_runtime` traps `EXIT_ENTRY` in `EXIT_IMAGE`, a library no macOS
/// process has; the same session without a runtime traps nothing there, and one given a
/// runtime naming another image does not trap it either.
#[test]
fn step_out_traps_the_exit_entries_the_runtime_names()
{
    use ferros_core::backend::LoadedImage;

    const BASE: u64 = 0x2_0000_0000;
    const ENTRY: u64 = BASE + 0x400;
    const TABLE: u64 = BASE + 0x1000;
    let symbols = MockObjectImage::new(ENTRY..ENTRY + 16, TABLE, TABLE + 0x100).with_symbol(ENTRY, EXIT_ENTRY);
    let mut image = symbols.header_bytes();
    image.resize(0x1000, 0);
    image.extend(symbols.table_bytes());
    image.resize(0x1100, 0);
    image.extend(symbols.name_bytes());
    let mock = || {
        returning_to_breakpoint()
            .with_region(Address::new(BASE), "r-x", image.clone())
            .with_image(LoadedImage {
                path: format!("/usr/lib/{EXIT_IMAGE}").into(),
                load_address: Address::new(BASE),
                build_id: ferros_core::BuildIdentity::Unknown,
                role: ferros_core::backend::ImageRole::Library,
            })
    };
    let trap = arch::Arm64::TRAP_INSTRUCTION.to_vec();
    let stepped_out = |runtime: Option<std::sync::Arc<dyn ferros_core::runtime::TargetRuntime>>| {
        let mock = mock();
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        if let Some(runtime) = runtime {
            session = session.with_runtime(runtime);
        }
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        step_out_to_breakpoint(&mock, &mut session);
        mock.peek_memory(Address::new(ENTRY), 4).expect("entry bytes")
    };

    assert_eq!(stepped_out(Some(exit_runtime())), trap, "the runtime's exit entry is trapped");
    assert_ne!(stepped_out(None), trap, "no runtime, no exit trap");
    let elsewhere = MockRuntime::new()
        .with_exit_source(ExitSource::named("libother.so", &[EXIT_ENTRY]))
        .into_shared();
    assert_ne!(stepped_out(Some(elsewhere)), trap, "another image's entry is not this one");
}

/// #201: a signal trampoline is recognized by the session's runtime. The handler's frame
/// record returns into `MockRuntime`'s trampoline, whose saved state (at its `fp + 16`)
/// holds the interrupted frame; a trace through the session restores it. Without the
/// runtime the trace follows the trampoline's frame record instead and misses it.
#[test]
fn a_backtrace_restores_the_frame_the_runtimes_signal_trampoline_interrupted()
{
    const CODE: u64 = 0x4000_0000;
    const TRAMPOLINE: u64 = 0x5000_0000;
    const STACK_BASE: u64 = 0x1000;
    const HANDLER_FP: u64 = 0x1000;
    const TRAMPOLINE_FP: u64 = 0x1100;
    const CONTEXT: u64 = TRAMPOLINE_FP + MockRuntime::CONTEXT_OFFSET;
    const INTERRUPTED_SP: u64 = 0x2000;
    const INTERRUPTED_FP: u64 = 0x2100;
    const INTERRUPTED_PC: u64 = CODE + 0x100;

    let mut stack = vec![0u8; 0x2000];
    let mut put = |at: u64, value: u64| {
        let offset = usize::try_from(at - STACK_BASE).expect("offset");
        stack[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };
    // The handler's frame record returns into the trampoline.
    put(HANDLER_FP, TRAMPOLINE_FP);
    put(HANDLER_FP + 8, TRAMPOLINE + 0x44);
    // The trampoline's record holds the interrupted fp and a stale link register.
    put(TRAMPOLINE_FP, INTERRUPTED_FP);
    put(TRAMPOLINE_FP + 8, CODE + 0xaa0);
    for index in 0..MockRuntime::GENERAL_WORDS {
        put(CONTEXT + 8 * index, 0x100 + index);
    }
    put(CONTEXT + MockRuntime::FP, INTERRUPTED_FP);
    put(CONTEXT + MockRuntime::LR, CODE + 0xaa0);
    put(CONTEXT + MockRuntime::SP, INTERRUPTED_SP);
    put(CONTEXT + MockRuntime::PC, INTERRUPTED_PC);
    // The interrupted frame's record ends the chain at its caller.
    put(INTERRUPTED_FP, 0);
    put(INTERRUPTED_FP + 8, CODE + 0x200);

    let mut registers = arm64_registers();
    registers.pc = Address::new(CODE);
    registers.sp = Address::new(0xf00);
    registers.fp = Address::new(HANDLER_FP);
    registers.general[29] = HANDLER_FP;
    let mock = MockBackend::new()
        .with_thread(THREAD, registers)
        .with_region(Address::new(CODE), "r-x", vec![0x1F; 0x1000])
        .with_region(Address::new(TRAMPOLINE), "r-x", vec![0x1F; 0x100])
        .with_region(Address::new(0), "rw-", vec![0; 0x1000])
        .with_region(Address::new(STACK_BASE), "rw-", stack);
    let pcs = |runtime: Option<std::sync::Arc<dyn ferros_core::runtime::TargetRuntime>>| {
        let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
        if let Some(runtime) = runtime {
            session = session.with_runtime(runtime);
        }
        session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
        let frames = session.stack_trace(16).expect("stack trace");
        frames.iter().map(|frame| frame.pc.value()).collect::<Vec<_>>()
    };

    let runtime = MockRuntime::new()
        .with_signal_trampoline(TRAMPOLINE..TRAMPOLINE + 0x100)
        .into_shared();
    assert_eq!(
        pcs(Some(runtime)),
        [CODE, TRAMPOLINE + 0x44, INTERRUPTED_PC, CODE + 0x200],
        "the interrupted frame is restored"
    );
    // Without the runtime the trampoline's record is followed: its stale link register
    // stands in for the interrupted frame, which is never reported.
    let unaware = pcs(None);
    assert_eq!(unaware, [CODE, TRAMPOLINE + 0x44, CODE + 0xaa0, CODE + 0x200]);
    assert!(!unaware.contains(&INTERRUPTED_PC), "{unaware:x?}");
}
