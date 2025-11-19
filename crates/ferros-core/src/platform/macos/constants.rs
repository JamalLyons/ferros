//! # macOS Platform Constants
//!
//! Centralized constants for macOS Mach API operations.
//!
//! This module contains all platform-specific constants used throughout the
//! macOS debugger implementation, including thread state flavors, debug state
//! flavors, instruction sizes, and other magic numbers.
//!
//! ## Organization
//!
//! Constants are organized by category:
//! - Thread state flavors and counts
//! - Debug state flavors and counts
//! - Instruction sizes
//! - Memory operation constants
//! - Breakpoint trap instructions
//! - Register layout indices
//! - Bit masks and magic values

use libc::{c_int, mach_msg_type_number_t};

// ============================================================================
// Thread State Flavors
// ============================================================================

/// ARM64 thread state flavor (flavor 6)
///
/// Used with `thread_get_state()` and `thread_set_state()` to read/write
/// ARM64 general-purpose registers (X0-X30, SP, PC, CPSR).
///
/// See: [ARM_THREAD_STATE64](https://developer.arm.com/documentation/101407/0543/Debugging/Debug-Windows-and-Dialogs/System-and-Thread-Viewer/Thread-States)
#[cfg(target_arch = "aarch64")]
pub const ARM_THREAD_STATE64: c_int = 6;

/// ARM64 thread state count (68 u32 values)
///
/// The number of `natural_t` (u32) values required to hold ARM64 thread state.
/// Each 64-bit register is stored as two u32 values.
#[cfg(target_arch = "aarch64")]
pub const ARM_THREAD_STATE64_COUNT: mach_msg_type_number_t = 68;

/// x86-64 thread state flavor (flavor 4)
///
/// Used with `thread_get_state()` and `thread_set_state()` to read/write
/// x86-64 general-purpose registers (RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP,
/// R8-R15, RIP, RFLAGS, CS, FS, GS).
#[cfg(target_arch = "x86_64")]
pub const X86_THREAD_STATE64: c_int = 4;

/// x86-64 thread state count (42 u32 values)
///
/// The number of `natural_t` (u32) values required to hold x86-64 thread state.
#[cfg(target_arch = "x86_64")]
pub const X86_THREAD_STATE64_COUNT: mach_msg_type_number_t = 42;

/// ARM64 NEON (floating-point) state flavor (flavor 17)
///
/// Used to read/write ARM64 NEON/SIMD registers (V0-V31) and floating-point
/// status registers (FPSR, FPCR).
#[cfg(target_arch = "aarch64")]
pub const ARM_NEON_STATE64: c_int = 17;

/// ARM64 NEON state count (520 bytes / 4 = 130 u32 values)
///
/// The number of `natural_t` (u32) values required to hold ARM64 NEON state.
#[cfg(target_arch = "aarch64")]
pub const ARM_NEON_STATE64_COUNT: mach_msg_type_number_t = 130;

/// x86-64 floating-point state flavor (flavor 5)
///
/// Used to read/write x86-64 floating-point registers (XMM0-XMM15, MXCSR).
#[cfg(target_arch = "x86_64")]
pub const X86_FLOAT_STATE64: c_int = 5;

/// x86-64 floating-point state count (512 bytes / 4 = 128 u32 values)
///
/// The number of `natural_t` (u32) values required to hold x86-64 floating-point state.
#[cfg(target_arch = "x86_64")]
pub const X86_FLOAT_STATE64_COUNT: mach_msg_type_number_t = 128;

// ============================================================================
// Debug State Flavors
// ============================================================================

/// x86-64 debug state flavor (flavor 11)
///
/// Used with `thread_get_state()` and `thread_set_state()` to read/write
/// x86-64 debug registers (DR0-DR7) for hardware breakpoints.
#[cfg(target_arch = "x86_64")]
pub const X86_DEBUG_STATE64: c_int = 11;

/// x86-64 debug state count (64 bytes / 4 = 16 u32 values)
///
/// The number of `natural_t` (u32) values required to hold x86-64 debug state.
#[cfg(target_arch = "x86_64")]
pub const X86_DEBUG_STATE64_COUNT: mach_msg_type_number_t = 16;

/// ARM64 debug state flavor (flavor 15)
///
/// Used with `thread_get_state()` and `thread_set_state()` to read/write
/// ARM64 debug registers (DBGBVR/DBGBCR for breakpoints, DBGWVR/DBGWCR for watchpoints).
#[cfg(target_arch = "aarch64")]
pub const ARM_DEBUG_STATE64: c_int = 15;

/// ARM64 debug state count (520 bytes / 4 = 130 u32 values)
///
/// The number of `natural_t` (u32) values required to hold ARM64 debug state.
#[cfg(target_arch = "aarch64")]
pub const ARM_DEBUG_STATE64_COUNT: mach_msg_type_number_t = 130;

// ============================================================================
// Instruction Sizes
// ============================================================================

/// ARM64 instruction size in bytes
///
/// ARM64 uses fixed-length 32-bit (4-byte) instructions.
#[cfg(target_arch = "aarch64")]
pub const ARM64_INSTRUCTION_SIZE: u64 = 4;

/// x86-64 instruction size in bytes
///
/// x86-64 uses variable-length instructions, but for breakpoint rewinding
/// we use 1 byte (the size of INT3).
#[cfg(target_arch = "x86_64")]
pub const X86_64_INSTRUCTION_SIZE: u64 = 1;

// ============================================================================
// Memory Operation Constants
// ============================================================================

/// Maximum chunk size for `vm_read()` operations (64 KB)
///
/// macOS `vm_read()` has limitations on how much memory can be read in a
/// single call. We chunk larger reads into 64 KB pieces.
pub const MAX_VM_READ_CHUNK: usize = 64 * 1024;

/// Chunk size for pattern scanning operations (64 KB)
///
/// When searching for byte patterns in memory, we process memory in
/// 64 KB chunks to avoid excessive memory allocation.
pub const PATTERN_SCAN_CHUNK: usize = 64 * 1024;

// ============================================================================
// Breakpoint Trap Instructions
// ============================================================================

/// ARM64 breakpoint instruction (`BRK #0`)
///
/// This is the 4-byte instruction sequence used for software breakpoints on ARM64.
/// The instruction is: `BRK #0` encoded as `0x00, 0x00, 0x20, 0xD4`.
#[cfg(target_arch = "aarch64")]
pub const ARM64_BRK_INSTRUCTION: &[u8] = &[0x00, 0x00, 0x20, 0xD4];

/// x86-64 breakpoint instruction (`INT3`)
///
/// This is the 1-byte instruction used for software breakpoints on x86-64.
/// The instruction is: `INT3` encoded as `0xCC`.
#[cfg(target_arch = "x86_64")]
pub const X86_64_INT3_INSTRUCTION: &[u8] = &[0xCC];

// ============================================================================
// ARM64 Breakpoint Control Register Values
// ============================================================================

/// ARM64 breakpoint control register value for user-mode execution breakpoint
///
/// This value configures a hardware breakpoint to:
/// - Enable the breakpoint (bit 0 = 1)
/// - Match in user mode (PMC = 10, bits 1-2)
/// - Match all bytes (BAS = 1111, bits 5-8)
///
/// Value: `0x1E5`
///
/// Bit breakdown:
/// - Bit 0 (E): 1 (enabled)
/// - Bits 1-2 (PMC): 10 (user mode)
/// - Bits 5-8 (BAS): 1111 (match all bytes)
#[cfg(target_arch = "aarch64")]
pub const ARM64_BP_CTRL_USER_EXEC: u64 = 0x1E5;

// ============================================================================
// Bit Masks
// ============================================================================

/// Mask for extracting the lower 32 bits of a u64
///
/// Used when splitting 64-bit values into two 32-bit values for Mach APIs.
pub const U32_MASK: u64 = 0xFFFF_FFFF;

// ============================================================================
// ARM64 Register Layout Indices
// ============================================================================

/// ARM64 register array index for X0 (first general-purpose register)
///
/// In the ARM64 thread state array, general-purpose registers X0-X30 are
/// stored at indices 0-30 (each register takes 2 u32 values).
#[cfg(target_arch = "aarch64")]
pub const ARM64_X0_INDEX: usize = 0;

/// ARM64 register array index for FP (Frame Pointer, X29)
///
/// The frame pointer is stored at index 29 in the ARM64 thread state array.
#[cfg(target_arch = "aarch64")]
pub const ARM64_FP_INDEX: usize = 29;

/// ARM64 register array index for LR (Link Register, X30)
///
/// The link register (return address) is stored at index 30 in the ARM64 thread state array.
#[cfg(target_arch = "aarch64")]
pub const ARM64_LR_INDEX: usize = 30;

/// ARM64 register array index for SP (Stack Pointer)
///
/// The stack pointer is stored at index 31 in the ARM64 thread state array.
#[cfg(target_arch = "aarch64")]
pub const ARM64_SP_INDEX: usize = 31;

/// ARM64 register array index for PC (Program Counter)
///
/// The program counter is stored at index 32 in the ARM64 thread state array.
/// It occupies two u32 values (indices 64-65 in the raw state_words array).
#[cfg(target_arch = "aarch64")]
pub const ARM64_PC_INDEX: usize = 32;

/// ARM64 register array index for PC low 32 bits (in state_words array)
///
/// The PC is stored as two u32 values. This is the index of the low 32 bits.
#[cfg(target_arch = "aarch64")]
pub const ARM64_PC_INDEX_LOW: usize = 64;

/// ARM64 register array index for PC high 32 bits (in state_words array)
///
/// The PC is stored as two u32 values. This is the index of the high 32 bits.
#[cfg(target_arch = "aarch64")]
pub const ARM64_PC_INDEX_HIGH: usize = 65;

/// ARM64 register array index for CPSR (Current Program Status Register)
///
/// The CPSR is stored at index 66 in the ARM64 thread state array (state_words).
/// It's a single u32 value (the second u32 at index 67 is padding).
#[cfg(target_arch = "aarch64")]
pub const ARM64_CPSR_INDEX: usize = 66;
