//! Unit tests for [`Session`] on the scripted [`MockBackend`].

use std::time::Duration;

use super::*;
use crate::arch::Arm64;
use crate::backend::mock::{MockBackend, MockCall, MockCallKind, MockState, MockStop, arm64_registers};
use crate::backend::{BackendError, LaunchSpec, LoadedImage, ResumeRequest, ThreadAction};
use crate::breakpoints::builder::BreakpointBuilder;
use crate::breakpoints::{BreakpointId, BreakpointRequest, BreakpointState, WatchpointAccess};
use crate::objfile::mock::MockObjectFormat;

const T1: ThreadId = ThreadId(1);
const T2: ThreadId = ThreadId(2);
const CODE: u64 = 0x1000;
const BP: u64 = 0x1008;
const BP2: u64 = 0x1010;
const TRAP: [u8; 4] = Arm64::TRAP_INSTRUCTION;

fn regs_at(pc: u64) -> Registers
{
    let mut registers = arm64_registers();
    registers.pc = Address::new(pc);
    registers
}

/// A mock process with two threads, 32 bytes of code (bytes 0..32) and one image.
fn process() -> MockBackend
{
    MockBackend::new()
        .with_pid(ProcessId(42))
        .with_thread(T1, regs_at(CODE))
        .with_thread(T2, regs_at(CODE + 4))
        .with_region(Address::new(CODE), "r-x", (0u8..32).collect::<Vec<_>>())
}

fn spec() -> LaunchSpec
{
    LaunchSpec::new("/bin/prog")
}

fn session(mock: &MockBackend) -> Session
{
    Session::new(Box::new(mock.clone()), MockObjectFormat::shared())
}

fn launched(mock: &MockBackend) -> Session
{
    let mut session = session(mock);
    session.launch(&spec()).expect("launch");
    let _ = mock.take_calls();
    session
}

fn software(address: u64) -> BreakpointRequest
{
    BreakpointRequest::Software {
        address: Address::new(address),
    }
}

fn original(address: u64) -> Vec<u8>
{
    (address - CODE..address - CODE + 4).map(|b| b as u8).collect()
}

fn peek(mock: &MockBackend, address: u64) -> Vec<u8>
{
    mock.peek_memory(Address::new(address), 4).expect("mapped")
}

fn is_unsupported(result: &Result<BreakpointId>) -> bool
{
    matches!(result, Err(DebuggerError::Backend(BackendError::Unsupported(_))))
}

/// A neutral mock image header with one segment (file address 0x1_0000_0000, 0x1000
/// bytes, the header at its start), as the loader would map it.
fn mapped_text_header() -> Vec<u8>
{
    crate::objfile::mock::MockObjectImage::new(0..1, 0, 0)
        .with_mapping(0x1_0000_0000, 0x1000)
        .header_bytes()
}

#[path = "tests/breakpoints.rs"]
mod breakpoints;

#[path = "tests/lifecycle.rs"]
mod lifecycle;

#[path = "tests/logical.rs"]
mod logical;

#[path = "tests/passthrough.rs"]
mod passthrough;

#[path = "tests/resume_wait.rs"]
mod resume_wait;

#[path = "tests/stack.rs"]
mod stack;

#[path = "tests/stepping.rs"]
mod stepping;

#[path = "tests/symbol_tables.rs"]
mod symbol_tables;

#[path = "tests/process_control.rs"]
mod process_control;

#[path = "tests/frames.rs"]
mod frames;
