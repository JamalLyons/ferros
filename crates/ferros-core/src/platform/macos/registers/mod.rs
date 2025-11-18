//! # macOS Register Reading
//!
//! Functions to read CPU registers from macOS processes using Mach APIs.
//!
//! On macOS, registers are read using `thread_get_state()` with architecture-specific
//! "flavors" that specify which registers to read:
//!
//! - **ARM64**: `ARM_THREAD_STATE64` (flavor 6)
//! - **x86-64**: `X86_THREAD_STATE64` (flavor 4)
//!
//! ## Register Layout
//!
//! macOS stores registers as arrays of `natural_t` (which is `u32`). Each 64-bit
//! register is stored as two `u32` values in little-endian format:
//!
//! - Low 32 bits at index `i * 2`
//! - High 32 bits at index `i * 2 + 1`
//!
//! ## References
//!
//! - [thread_get_state documentation](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
//! - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
//! - [ARM_THREAD_STATE64 structure](https://opensource.apple.com/source/xnu/xnu-4570.71.2/osfmk/mach/arm/_structs.h)

#[cfg(target_arch = "aarch64")]
pub mod arm64;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

pub mod debug;

// Re-export architecture-specific functions
#[cfg(target_arch = "aarch64")]
pub use arm64::{read_registers_arm64, write_registers_arm64};
// Re-export debug register functions
pub use debug::{clear_hardware_breakpoint, set_hardware_breakpoint};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{read_registers_x86_64, write_registers_x86_64};
