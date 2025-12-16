//! CPU register types and access.

use crate::types::address::Address;
use crate::types::process::Architecture;

/// Identifier for a specific CPU register
///
/// This enum provides a platform-agnostic way to identify registers across
/// different CPU architectures. It includes common registers (PC, SP, FP, Status)
/// that exist on all architectures, as well as architecture-specific registers
/// through the `Arm64` and `X86_64` variants.
///
/// ## Common Registers
///
/// - `Pc`: Program Counter (instruction pointer) - points to the next instruction
/// - `Sp`: Stack Pointer - points to the top of the stack
/// - `Fp`: Frame Pointer - points to the current stack frame
/// - `Status`: Status/Flags register - contains condition flags (carry, zero, etc.)
///
/// ## Architecture-Specific Registers
///
/// Use `Arm64(Arm64Register)` for ARM64-specific registers (X0-X30)
/// Use `X86_64(X86_64Register)` for x86-64-specific registers (RAX, RBX, etc.)
#[derive(Debug)]
pub enum RegisterId
{
    /// Program Counter (PC) - points to the next instruction to execute
    ///
    /// On ARM64, this is the PC register.
    /// On x86-64, this is the RIP (Instruction Pointer) register.
    Pc,
    /// Stack Pointer (SP) - points to the top of the stack
    ///
    /// On ARM64, this is the SP register.
    /// On x86-64, this is the RSP register.
    Sp,
    /// Frame Pointer (FP) - points to the current stack frame
    ///
    /// On ARM64, this is typically X29 (FP register).
    /// On x86-64, this is typically RBP (Base Pointer).
    Fp,
    /// Status/Flags register - contains CPU condition flags
    ///
    /// On ARM64, this is the CPSR (Current Program Status Register).
    /// On x86-64, this is the RFLAGS register.
    Status,
    /// ARM64-specific register identifier
    ///
    /// Use this variant to access ARM64 general-purpose registers (X0-X30).
    Arm64(Arm64Register),
    /// x86-64-specific register identifier
    ///
    /// Use this variant to access x86-64 general-purpose registers (RAX, RBX, etc.).
    X86_64(X86_64Register),
}

/// ARM64 general-purpose register identifier
///
/// ARM64 has 31 general-purpose registers named X0 through X30. This enum
/// represents these registers using a single variant that takes the register
/// number (0-30).
///
/// ## ARM64 Register Layout
///
/// - **X0-X28**: General-purpose registers (29 registers)
/// - **X29 (FP)**: Frame pointer (also accessible via `RegisterId::Fp`)
/// - **X30 (LR)**: Link register (return address)
///
/// Note: The stack pointer (SP) and program counter (PC) are special registers
/// that are accessed via `RegisterId::Sp` and `RegisterId::Pc` respectively.
///
/// ## References
///
/// - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
#[derive(Debug)]
pub enum Arm64Register
{
    /// General-purpose register X0-X30
    ///
    /// The value must be between 0 and 30 (inclusive). Values outside this range
    /// may cause undefined behavior when used with `Registers::get()` or `Registers::set()`.
    X(u8),
}

/// x86-64 general-purpose register identifier
///
/// x86-64 has 16 general-purpose registers. This enum provides named access
/// to these registers. Note that some registers have special purposes:
///
/// - **RAX**: Accumulator (return value register)
/// - **RBX**: Base register
/// - **RCX**: Counter (used in loops)
/// - **RDX**: Data register
/// - **RSI**: Source index (function arguments)
/// - **RDI**: Destination index (function arguments)
/// - **R8-R15**: Additional general-purpose registers (x86-64 extension)
///
/// Note: The stack pointer (RSP), base pointer (RBP), and instruction pointer (RIP)
/// are accessed via `RegisterId::Sp`, `RegisterId::Fp`, and `RegisterId::Pc` respectively.
#[derive(Debug, PartialEq)]
pub enum X86_64Register
{
    /// RAX - Accumulator register (often used for return values)
    Rax,
    /// RBX - Base register
    Rbx,
    /// RCX - Counter register (used in loops)
    Rcx,
    /// RDX - Data register
    Rdx,
    /// RSI - Source index register (function arguments)
    Rsi,
    /// RDI - Destination index register (function arguments)
    Rdi,
    /// R8 - General-purpose register (x86-64 extension)
    R8,
    /// R9 - General-purpose register (x86-64 extension)
    R9,
    /// R10 - General-purpose register (x86-64 extension)
    R10,
    /// R11 - General-purpose register (x86-64 extension)
    R11,
    /// R12 - General-purpose register (x86-64 extension)
    R12,
    /// R13 - General-purpose register (x86-64 extension)
    R13,
    /// R14 - General-purpose register (x86-64 extension)
    R14,
    /// R15 - General-purpose register (x86-64 extension)
    R15,
}

impl X86_64Register
{
    /// Get the index of this register in the general-purpose register array
    ///
    /// This method returns the index that should be used to access this register
    /// in the `Registers::general` vector. The indices are:
    ///
    /// - RAX = 0, RBX = 1, RCX = 2, RDX = 3
    /// - RSI = 4, RDI = 5
    /// - R8 = 6, R9 = 7, R10 = 8, R11 = 9
    /// - R12 = 10, R13 = 11, R14 = 12, R15 = 13
    ///
    /// This is an internal method used by `Registers::get()` and `Registers::set()`.
    const fn index(self) -> usize
    {
        match self {
            X86_64Register::Rax => 0,
            X86_64Register::Rbx => 1,
            X86_64Register::Rcx => 2,
            X86_64Register::Rdx => 3,
            X86_64Register::Rsi => 4,
            X86_64Register::Rdi => 5,
            X86_64Register::R8 => 6,
            X86_64Register::R9 => 7,
            X86_64Register::R10 => 8,
            X86_64Register::R11 => 9,
            X86_64Register::R12 => 10,
            X86_64Register::R13 => 11,
            X86_64Register::R14 => 12,
            X86_64Register::R15 => 13,
        }
    }
}

/// 128-bit SIMD register value.
///
/// This struct represents a 128-bit SIMD (Single Instruction, Multiple Data)
/// register value, which is used for vector operations on both ARM64 and x86-64.
/// SIMD registers are used for parallel processing of multiple data elements
/// (e.g., 4x 32-bit floats, 8x 16-bit integers, etc.).
///
/// ## Architecture Support
///
/// - **ARM64**: NEON registers (V0-V31) are 128-bit
/// - **x86-64**: XMM registers (XMM0-XMM15) are 128-bit
///
/// ## Byte Order
///
/// All values are stored in little-endian format, which is the native byte
/// order for both ARM64 and x86-64 architectures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorRegisterValue
{
    bytes: [u8; 16],
}

impl VectorRegisterValue
{
    /// Create a new vector register from raw bytes (little-endian).
    ///
    /// The bytes are stored as-is in little-endian format. The first byte
    /// (`bytes[0]`) is the least significant byte.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self
    {
        Self { bytes }
    }

    /// Create a vector register from a 128-bit integer (little-endian).
    ///
    /// The integer value is converted to bytes using little-endian byte order.
    /// The least significant byte of the integer becomes `bytes[0]`.
    #[must_use]
    pub const fn from_u128(value: u128) -> Self
    {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    /// Access the raw bytes of the vector register.
    ///
    /// Returns a reference to the 16-byte array containing the register value
    /// in little-endian format.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 16]
    {
        &self.bytes
    }

    /// Convert to a 128-bit integer (little-endian).
    ///
    /// The bytes are interpreted as a little-endian 128-bit unsigned integer.
    /// The first byte (`bytes[0]`) is the least significant byte.
    #[must_use]
    pub const fn as_u128(&self) -> u128
    {
        u128::from_le_bytes(self.bytes)
    }
}

/// Architecture-agnostic floating point status/control registers.
///
/// This struct holds floating-point and SIMD status/control register values
/// that are architecture-specific but conceptually similar across platforms.
/// These registers control floating-point rounding modes, exception flags,
/// and SIMD operation behavior.
///
/// ## Architecture-Specific Registers
///
/// - **ARM64**: FPSR (Floating-Point Status Register) and FPCR (Floating-Point Control Register)
/// - **x86-64**: MXCSR (MXCSR Register) for SSE/AVX operations
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FloatingPointState
{
    /// ARM64 FPSR (Floating-Point Status Register) or equivalent.
    ///
    /// Contains floating-point exception flags and condition flags.
    /// On ARM64, this is the FPSR register. On other architectures, this
    /// may be `None` or contain equivalent status information.
    pub fpsr: Option<u32>,
    /// ARM64 FPCR (Floating-Point Control Register) or equivalent.
    ///
    /// Controls floating-point rounding mode and exception enable bits.
    /// On ARM64, this is the FPCR register. On other architectures, this
    /// may be `None` or contain equivalent control information.
    pub fpcr: Option<u32>,
    /// x86-64 MXCSR (MXCSR Register) or equivalent.
    ///
    /// Controls SSE/AVX floating-point rounding mode, exception masks, and flags.
    /// On x86-64, this is the MXCSR register. On other architectures, this
    /// may be `None` or contain equivalent control information.
    pub mxcsr: Option<u32>,
}

/// Platform-agnostic register representation with architecture-aware helpers
///
/// This struct holds the CPU register values for a thread. It provides a unified
/// interface for accessing registers across different CPU architectures (ARM64, x86-64).
///
/// ## Register Fields
///
/// - `pc`: Program Counter (instruction pointer) - address of next instruction
/// - `sp`: Stack Pointer - address of top of stack
/// - `fp`: Frame Pointer - address of current stack frame
/// - `general`: Vector of general-purpose registers (architecture-specific)
/// - `status`: Status/Flags register (CPSR on ARM64, RFLAGS on x86-64)
/// - `architecture`: CPU architecture (used for architecture-specific register access)
///
/// ## Architecture-Specific Register Access
///
/// Use `get()` and `set()` with `RegisterId` to access architecture-specific registers:
#[derive(Debug)]
pub struct Registers
{
    /// Program Counter (PC) - address of the next instruction to execute
    pub pc: Address,
    /// Stack Pointer (SP) - address of the top of the stack
    pub sp: Address,
    /// Frame Pointer (FP) - address of the current stack frame
    pub fp: Address,
    /// General-purpose registers (architecture-specific)
    ///
    /// - **ARM64**: X0-X30 (31 registers)
    /// - **x86-64**: RAX, RBX, RCX, RDX, RSI, RDI, R8-R15 (14 registers)
    pub general: Vec<u64>,
    /// Status/Flags register
    ///
    /// - **ARM64**: CPSR (Current Program Status Register)
    /// - **x86-64**: RFLAGS (contains condition flags)
    pub status: u64,
    /// SIMD/vector registers (architecture-specific; 128-bit lanes).
    pub vector: Vec<VectorRegisterValue>,
    /// Floating-point status/control registers (architecture-specific metadata).
    pub floating: FloatingPointState,
    /// CPU architecture (used for architecture-specific register access)
    pub architecture: Architecture,
}

impl Registers
{
    /// Create a new empty `Registers` struct
    ///
    /// All registers are initialized to zero. You should set the architecture
    /// using `with_arch()` before accessing architecture-specific registers.
    pub fn new() -> Self
    {
        Self {
            pc: Address::ZERO,
            sp: Address::ZERO,
            fp: Address::ZERO,
            general: Vec::new(),
            status: 0,
            vector: Vec::new(),
            floating: FloatingPointState::default(),
            architecture: Architecture::Unknown("unknown"),
        }
    }

    /// Set the CPU architecture for this register set
    ///
    /// This method enables architecture-specific register access. You must call
    /// this before using `get()` or `set()` with architecture-specific `RegisterId` variants.
    pub fn with_arch(mut self, architecture: Architecture) -> Self
    {
        self.architecture = architecture;
        self
    }

    /// Get the CPU architecture for this register set
    ///
    /// Returns the architecture that was set when creating or modifying this register set.
    /// This determines which architecture-specific registers are available.
    pub fn architecture(&self) -> Architecture
    {
        self.architecture
    }

    /// Get the value of a register by its identifier
    ///
    /// Returns `Some(value)` if the register exists and is accessible, or `None` if:
    /// - The register identifier doesn't match the architecture (e.g., trying to access
    ///   ARM64 registers on an x86-64 register set)
    /// - The register index is out of bounds (for architecture-specific registers)
    pub fn get(&self, id: RegisterId) -> Option<u64>
    {
        match id {
            RegisterId::Pc => Some(self.pc.value()),
            RegisterId::Sp => Some(self.sp.value()),
            RegisterId::Fp => Some(self.fp.value()),
            RegisterId::Status => Some(self.status),
            RegisterId::Arm64(Arm64Register::X(idx)) => {
                if self.architecture != Architecture::Arm64 {
                    return None;
                }
                self.general.get(idx as usize).copied()
            }
            RegisterId::X86_64(reg) => {
                if self.architecture != Architecture::X86_64 {
                    return None;
                }
                let idx = reg.index();
                self.general.get(idx).copied()
            }
        }
    }

    /// Set the value of a register by its identifier
    ///
    /// Returns `Some(())` if the register was successfully set, or `None` if:
    /// - The register identifier doesn't match the architecture (e.g., trying to set
    ///   ARM64 registers on an x86-64 register set)
    /// - The register index is out of bounds (for architecture-specific registers)
    pub fn set(&mut self, id: RegisterId, value: u64) -> Option<()>
    {
        match id {
            RegisterId::Pc => {
                self.pc = Address::from(value);
                Some(())
            }
            RegisterId::Sp => {
                self.sp = Address::from(value);
                Some(())
            }
            RegisterId::Fp => {
                self.fp = Address::from(value);
                Some(())
            }
            RegisterId::Status => {
                self.status = value;
                Some(())
            }
            RegisterId::Arm64(Arm64Register::X(idx)) => {
                if self.architecture != Architecture::Arm64 {
                    return None;
                }
                let slot = self.general.get_mut(idx as usize)?;
                *slot = value;
                Some(())
            }
            RegisterId::X86_64(reg) => {
                if self.architecture != Architecture::X86_64 {
                    return None;
                }
                let slot = self.general.get_mut(reg.index())?;
                *slot = value;
                Some(())
            }
        }
    }

    /// Get a read-only view of the SIMD/vector registers.
    ///
    /// Returns a slice of all vector registers (NEON on ARM64, XMM on x86-64).
    /// The number of registers depends on the architecture:
    ///
    /// - **ARM64**: Up to 32 NEON registers (V0-V31)
    /// - **x86-64**: Up to 16 XMM registers (XMM0-XMM15) in 64-bit mode
    #[must_use]
    pub fn vector_registers(&self) -> &[VectorRegisterValue]
    {
        &self.vector
    }

    /// Get a mutable view of the SIMD/vector registers.
    ///
    /// Returns a mutable slice of all vector registers, allowing you to modify
    /// their values. The number of registers depends on the architecture.
    #[must_use]
    pub fn vector_registers_mut(&mut self) -> &mut [VectorRegisterValue]
    {
        &mut self.vector
    }

    /// Get the floating-point status/control state.
    ///
    /// Returns a reference to the floating-point state registers (FPSR, FPCR on ARM64,
    /// MXCSR on x86-64). These registers control floating-point rounding modes,
    /// exception flags, and SIMD operation behavior.
    #[must_use]
    pub fn floating_point_state(&self) -> &FloatingPointState
    {
        &self.floating
    }

    /// Get a mutable reference to the floating-point status/control state.
    ///
    /// Returns a mutable reference to the floating-point state registers, allowing
    /// you to modify rounding modes, exception flags, and SIMD operation behavior.
    #[must_use]
    pub fn floating_point_state_mut(&mut self) -> &mut FloatingPointState
    {
        &mut self.floating
    }
}

impl Default for Registers
{
    fn default() -> Self
    {
        Self::new()
    }
}
