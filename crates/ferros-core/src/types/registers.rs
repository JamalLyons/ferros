//! CPU register types and access.

use super::{Address, Architecture};

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
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::{Arm64Register, RegisterId, X86_64Register};
///
/// // Common registers
/// let pc = RegisterId::Pc;
/// let sp = RegisterId::Sp;
///
/// // Architecture-specific registers
/// let x0 = RegisterId::Arm64(Arm64Register::X(0));
/// let rax = RegisterId::X86_64(X86_64Register::Rax);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// ## Example
///
/// ```rust
/// use ferros_core::types::{Arm64Register, RegisterId};
///
/// // Access X0 register
/// let x0 = RegisterId::Arm64(Arm64Register::X(0));
///
/// // Access X29 (frame pointer) - same as RegisterId::Fp
/// let fp = RegisterId::Arm64(Arm64Register::X(29));
/// ```
///
/// ## References
///
/// - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::{RegisterId, X86_64Register};
///
/// // Access RAX register
/// let rax = RegisterId::X86_64(X86_64Register::Rax);
///
/// // Access R8 register
/// let r8 = RegisterId::X86_64(X86_64Register::R8);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorRegisterValue
{
    bytes: [u8; 16],
}

impl VectorRegisterValue
{
    /// Create a new vector register from raw bytes (little-endian).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self
    {
        Self { bytes }
    }

    /// Create a vector register from a 128-bit integer (little-endian).
    #[must_use]
    pub const fn from_u128(value: u128) -> Self
    {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    /// Access the raw bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 16]
    {
        &self.bytes
    }

    /// Convert to a 128-bit integer (little-endian).
    #[must_use]
    pub const fn as_u128(&self) -> u128
    {
        u128::from_le_bytes(self.bytes)
    }
}

/// Architecture-agnostic floating point status/control registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FloatingPointState
{
    /// ARM64 FPSR or equivalent (if available).
    pub fpsr: Option<u32>,
    /// ARM64 FPCR or equivalent (if available).
    pub fpcr: Option<u32>,
    /// x86 MXCSR (if available).
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
///
/// ```rust
/// use ferros_core::types::{Architecture, Arm64Register, RegisterId, Registers};
///
/// let mut regs = Registers::new().with_arch(Architecture::Arm64);
/// regs.general = vec![0; 31]; // Initialize ARM64 registers
/// regs.set(RegisterId::Arm64(Arm64Register::X(0)), 0x1234);
/// let x0 = regs.get(RegisterId::Arm64(Arm64Register::X(0)));
/// ```
///
/// ## Thread Safety
///
/// This struct is not thread-safe. If you need to share registers across threads,
/// wrap it in a `Mutex` or use channels to communicate.
#[derive(Debug, Clone)]
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
    architecture: Architecture,
}

impl Registers
{
    /// Create a new empty `Registers` struct
    ///
    /// All registers are initialized to zero. You should set the architecture
    /// using `with_arch()` before accessing architecture-specific registers.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Architecture, Registers};
    ///
    /// let regs = Registers::new().with_arch(Architecture::Arm64);
    /// ```
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
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Architecture, Registers};
    ///
    /// let regs = Registers::new().with_arch(Architecture::Arm64);
    /// assert_eq!(regs.architecture(), Architecture::Arm64);
    /// ```
    pub fn with_arch(mut self, architecture: Architecture) -> Self
    {
        self.architecture = architecture;
        self
    }

    /// Get the CPU architecture for this register set
    ///
    /// Returns the architecture that was set when creating or modifying this register set.
    /// This determines which architecture-specific registers are available.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Architecture, Registers};
    ///
    /// let regs = Registers::new().with_arch(Architecture::X86_64);
    /// assert_eq!(regs.architecture(), Architecture::X86_64);
    /// ```
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
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Architecture, Arm64Register, RegisterId, Registers};
    ///
    /// let mut regs = Registers::new().with_arch(Architecture::Arm64);
    /// regs.general = vec![0x1234; 31]; // Initialize ARM64 registers
    ///
    /// // Access common registers
    /// let pc = regs.get(RegisterId::Pc);
    ///
    /// // Access architecture-specific registers
    /// let x0 = regs.get(RegisterId::Arm64(Arm64Register::X(0)));
    /// ```
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
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Address, Architecture, Arm64Register, RegisterId, Registers};
    ///
    /// let mut regs = Registers::new().with_arch(Architecture::Arm64);
    /// regs.general = vec![0; 31]; // Initialize ARM64 registers
    ///
    /// // Set common registers
    /// regs.set(RegisterId::Pc, 0x1000);
    ///
    /// // Set architecture-specific registers
    /// regs.set(RegisterId::Arm64(Arm64Register::X(0)), 0x1234);
    /// ```
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

    pub(crate) fn set_architecture(&mut self, architecture: Architecture)
    {
        self.architecture = architecture;
    }

    /// Read-only view of the SIMD/vector registers.
    #[must_use]
    pub fn vector_registers(&self) -> &[VectorRegisterValue]
    {
        &self.vector
    }

    /// Mutable view of the SIMD/vector registers.
    #[must_use]
    pub fn vector_registers_mut(&mut self) -> &mut [VectorRegisterValue]
    {
        &mut self.vector
    }

    /// Floating point status/control state.
    #[must_use]
    pub fn floating_point_state(&self) -> &FloatingPointState
    {
        &self.floating
    }

    /// Mutable floating point state.
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
