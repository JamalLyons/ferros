//! #222: the session takes its architecture knowledge from the backend
//! (`Backend::arch`), so a target's own address width and unwind conventions reach every
//! decision: breakpoint stops in `handle_stop`, trap bytes and backtraces.

use std::sync::Arc;

use ferros_core::arch::{Arch, Arm64, UndefinedReturnAddress};
use ferros_core::backend::mock::{MockBackend, MockStop, arm64_registers};
use ferros_core::backend::{Backend, BackendResult, ImageRole, LaunchSpec, LoadedImage, StopEvent};
use ferros_core::objfile::mock::{MockObjectFile, MockObjectFormat, MockObjectImage};
use ferros_core::types::{Address, Architecture, BuildIdentity, Registers};
use ferros_core::{BreakpointRequest, Session};

use crate::helpers::*;

/// A return address with bit 48 set: a 47-bit mask clears it, a 52-bit one keeps it.
const HIGH_RETURN: Address = Address::new(0x0001_0000_0000_1000);

/// A target whose user pointers have 52 significant bits keeps bit 48 of a saved return
/// address. With the 47-bit default the caller's pc would become `0x1000`, which is not
/// executable, and the trace would end after the innermost frame.
#[test]
fn configured_address_width_keeps_bit_48_in_a_session_backtrace()
{
    let mut stack = vec![0u8; 32];
    stack[8..16].copy_from_slice(&HIGH_RETURN.value().to_le_bytes());
    let mock = MockBackend::new()
        .with_arch(Arm64::with_addressable_bits(52).expect("valid width"))
        .with_thread(THREAD, in_callee(arm64_registers()))
        .with_region(CALLEE, "r-x", vec![0x1F; 64])
        .with_region(HIGH_RETURN, "r-x", vec![0x1F; 64])
        .with_region(STACK, "rw-", stack);
    let mut session = Session::new(Box::new(mock), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");

    let pcs: Vec<Address> = session
        .stack_trace(8)
        .expect("stack trace")
        .iter()
        .map(|frame| frame.pc)
        .collect();
    assert_eq!(pcs, vec![CALLEE, HIGH_RETURN]);
}

/// Arm64 with a configurable callee-saved mask and traps reported one instruction
/// past the `BRK`.
#[derive(Debug)]
struct ConfiguredArm64
{
    base: Arm64,
    callee_saved_mask: u32,
}

impl Arch for ConfiguredArm64
{
    fn architecture(&self) -> Architecture
    {
        self.base.architecture()
    }

    fn trap_instruction(&self) -> &'static [u8]
    {
        self.base.trap_instruction()
    }

    fn pc(&self, registers: &Registers) -> Address
    {
        self.base.pc(registers)
    }

    fn set_pc(&self, registers: &mut Registers, value: Address)
    {
        self.base.set_pc(registers, value);
    }

    fn sp(&self, registers: &Registers) -> Address
    {
        self.base.sp(registers)
    }

    fn set_sp(&self, registers: &mut Registers, value: Address)
    {
        self.base.set_sp(registers, value);
    }

    fn fp(&self, registers: &Registers) -> Address
    {
        self.base.fp(registers)
    }

    fn set_fp(&self, registers: &mut Registers, value: Address)
    {
        self.base.set_fp(registers, value);
    }

    fn link_register(&self, registers: &Registers) -> Option<Address>
    {
        self.base.link_register(registers)
    }

    fn set_link_register(&self, registers: &mut Registers, value: Address) -> BackendResult<()>
    {
        self.base.set_link_register(registers, value)
    }

    fn pc_after_trap(&self, reported_pc: Address) -> Address
    {
        Address::new(reported_pc.value() - 4)
    }

    fn strip_pointer_auth(&self, address: Address) -> Address
    {
        self.base.strip_pointer_auth(address)
    }

    fn callee_saved_register_mask(&self) -> u32
    {
        self.callee_saved_mask
    }
}

/// `handle_stop` maps a breakpoint stop's pc through the backend's `Arch`, the same
/// resolver as every other caller: the hit is counted and reported at the trap address.
#[test]
fn breakpoint_stops_use_the_backend_configured_arch()
{
    let arm64 = Arm64::new();
    let mock = MockBackend::new()
        .with_arch(ConfiguredArm64 {
            base: arm64,
            callee_saved_mask: arm64.callee_saved_register_mask(),
        })
        .with_thread(THREAD, arm64_registers())
        .with_region(CALLEE, "r-x", vec![0x1F; 64]);
    let mut session = Session::new(Box::new(mock.clone()), MockObjectFormat::shared());
    session.launch(&LaunchSpec::new("/bin/prog")).expect("launch");
    let id = session
        .add_breakpoint(BreakpointRequest::Software { address: BREAKPOINT })
        .expect("breakpoint");
    mock.script(
        MockStop::new(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT + 4,
        })
        .set_pc(THREAD, BREAKPOINT + 4),
    );
    session.resume().expect("resume");
    assert_eq!(
        session.wait(None).expect("wait"),
        Some(StopEvent::Breakpoint {
            thread: THREAD,
            pc: BREAKPOINT,
        })
    );
    assert_eq!(session.breakpoint_info(id).expect("breakpoint").hit_count, 1);
}

/// The first CFI row leaves X3 alone. The caller's row uses it as a return address,
/// so only a target ABI that preserves X3 can unwind both steps.
#[test]
fn session_unwind_uses_the_backend_callee_saved_mask()
{
    use gimli::write::{
        CallFrameInstruction, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable,
    };
    use gimli::{AArch64, Encoding, Format, LittleEndian};

    let pc = CALLEE + 8;
    let caller = CALLEE + 0x28;
    let outer = CALLEE + 0x48;
    let arm64 = Arm64::new();
    let mut regs = arm64_registers();
    regs.pc = pc;
    regs.sp = STACK;
    arm64.set_link_register(&mut regs, caller).expect("set link register");
    regs.general[usize::from(AArch64::X3.0)] = outer.value();

    let encoding = Encoding {
        address_size: 8,
        format: Format::Dwarf32,
        version: 1,
    };
    let mut cie = CommonInformationEntry::new(encoding, 4, -8, AArch64::X30);
    cie.add_instruction(CallFrameInstruction::Cfa(AArch64::SP, 16));

    let mut table = FrameTable::default();
    let cie = table.add_cie(cie);
    let mut inner_fde = FrameDescriptionEntry::new(gimli::write::Address::Constant(CALLEE.value()), 0x20);
    inner_fde.add_instruction(0, CallFrameInstruction::SameValue(AArch64::X30));
    table.add_fde(cie, inner_fde);
    let mut caller_fde = FrameDescriptionEntry::new(gimli::write::Address::Constant((CALLEE + 0x20).value()), 0x20);
    caller_fde.add_instruction(0, CallFrameInstruction::Register(AArch64::X30, AArch64::X3));
    table.add_fde(cie, caller_fde);

    let mut eh_frame = EhFrame::from(EndianVec::new(LittleEndian));
    table.write_eh_frame(&mut eh_frame).expect("write CFI");
    let image = MockObjectFile::new(IMAGE_BASE.value(), 0x2000)
        .with_eh_frame(IMAGE_BASE.value(), eh_frame.slice())
        .to_bytes();
    let (mut file, path) = ferros_tests::scratch::create_file("ferros-arch-callee-saved", ".mock");
    std::io::Write::write_all(&mut file, &image).expect("write synthetic image");
    drop(file);

    let mut mapped = MockObjectImage::new(0..1, 0, 0)
        .with_mapping(IMAGE_BASE.value(), 0x2000)
        .header_bytes();
    mapped.resize(0x2000, 0x1F);
    let unwind = |callee_saved_mask| {
        let mock = MockBackend::new()
            .with_arch(ConfiguredArm64 {
                base: arm64,
                callee_saved_mask,
            })
            .with_thread(THREAD, regs.clone())
            .with_region(IMAGE_BASE, "r-x", mapped.clone())
            .with_image(LoadedImage {
                path: path.clone(),
                load_address: IMAGE_BASE,
                build_id: BuildIdentity::Unknown,
                role: ImageRole::MainExecutable,
            });
        let mut session = Session::new(Box::new(mock), MockObjectFormat::shared());
        session.launch(&LaunchSpec::new("/bin/prog")).expect("mock launch");
        session
            .stack_trace(8)
            .expect("stack trace")
            .into_iter()
            .map(|frame| frame.pc)
            .collect::<Vec<_>>()
    };

    let standard_mask = arm64.callee_saved_register_mask();
    let standard = unwind(standard_mask);
    let with_x3_saved = unwind(standard_mask | (1 << AArch64::X3.0));
    std::fs::remove_file(path).expect("remove synthetic image");
    assert_eq!(standard, [pc, caller]);
    assert_eq!(with_x3_saved, [pc, caller, outer]);
}

/// The macOS backend configures today's conventions explicitly: a 47-bit user address
/// space and "an undefined return address is still in `lr`".
#[cfg(target_os = "macos")]
#[test]
fn macos_configures_a_47_bit_mask_and_the_link_register_rule()
{
    let backend = ferros_macos::MacOSBackend::new().expect("backend");
    let arch: Arc<dyn Arch> = backend.arch().expect("arm64 arch");
    assert_eq!(arch.architecture(), Architecture::Arm64);
    assert_eq!(
        arch.strip_pointer_auth(Address::new(0x0000_C000_0000_1000)),
        Address::new(0x0000_4000_0000_1000),
        "bit 46 is kept, bit 47 is cleared"
    );
    assert_eq!(arch.undefined_return_address(), UndefinedReturnAddress::LinkRegister);
    assert_eq!(ferros_macos::task::ARM64_ADDRESSABLE_BITS, 47);
}
