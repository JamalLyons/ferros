//! #222: the session takes its architecture knowledge from the backend
//! (`Backend::arch`), so a target's own address width and unwind conventions reach every
//! decision: breakpoint stops in `handle_stop`, trap bytes and backtraces.

use std::sync::Arc;

use ferros_core::arch::{Arch, Arm64, UndefinedReturnAddress};
use ferros_core::backend::mock::{MockBackend, MockStop, arm64_registers};
use ferros_core::backend::{Backend, BackendResult, LaunchSpec, StopEvent};
use ferros_core::objfile::mock::MockObjectFormat;
use ferros_core::types::{Address, Architecture, Registers};
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

/// Arm64 whose trap is reported one instruction past the `BRK`, so a session that maps
/// the reported pc through any other `Arch` misses the hit.
#[derive(Debug)]
struct TrapReportedPastBrk(Arm64);

impl Arch for TrapReportedPastBrk
{
    fn architecture(&self) -> Architecture
    {
        self.0.architecture()
    }

    fn trap_instruction(&self) -> &'static [u8]
    {
        self.0.trap_instruction()
    }

    fn pc(&self, registers: &Registers) -> Address
    {
        self.0.pc(registers)
    }

    fn set_pc(&self, registers: &mut Registers, value: Address)
    {
        self.0.set_pc(registers, value);
    }

    fn sp(&self, registers: &Registers) -> Address
    {
        self.0.sp(registers)
    }

    fn set_sp(&self, registers: &mut Registers, value: Address)
    {
        self.0.set_sp(registers, value);
    }

    fn fp(&self, registers: &Registers) -> Address
    {
        self.0.fp(registers)
    }

    fn set_fp(&self, registers: &mut Registers, value: Address)
    {
        self.0.set_fp(registers, value);
    }

    fn link_register(&self, registers: &Registers) -> Option<Address>
    {
        self.0.link_register(registers)
    }

    fn set_link_register(&self, registers: &mut Registers, value: Address) -> BackendResult<()>
    {
        self.0.set_link_register(registers, value)
    }

    fn pc_after_trap(&self, reported_pc: Address) -> Address
    {
        Address::new(reported_pc.value() - 4)
    }

    fn strip_pointer_auth(&self, address: Address) -> Address
    {
        self.0.strip_pointer_auth(address)
    }
}

/// `handle_stop` maps a breakpoint stop's pc through the backend's `Arch`, the same
/// resolver as every other caller: the hit is counted and reported at the trap address.
#[test]
fn breakpoint_stops_use_the_backend_configured_arch()
{
    let mock = MockBackend::new()
        .with_arch(TrapReportedPastBrk(Arm64::new()))
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
