//! # Types
//!
//! Platform-agnostic types used throughout the debugger.
//!
//! These types abstract away platform-specific details, allowing the rest of
//! the debugger to work with concepts like "process ID" and "registers" without
//! knowing whether we're on macOS, Linux, or Windows.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl Default for VectorRegisterValue
{
    fn default() -> Self
    {
        Self { bytes: [0; 16] }
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

use std::fmt;
use std::ops::{Add, Sub};

/// Process identifier (PID)
///
/// A PID is a unique number assigned to each running process by the operating
/// system. On Unix-like systems (macOS, Linux), PIDs are typically 32-bit
/// unsigned integers.
///
/// ## Why wrap it in a struct?
///
/// Using a newtype pattern (`struct ProcessId(u32)`) instead of a raw `u32`
/// provides:
/// - **Type safety**: Prevents accidentally passing a random number where a PID is expected
/// - **Self-documenting code**: Makes it clear what the value represents
/// - **Future extensibility**: Can add methods or validation later
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::platform::macos::MacOSDebugger;
/// use ferros_core::types::ProcessId;
/// use ferros_core::Debugger;
///
/// let pid = ProcessId::from(12345);
/// let mut debugger = MacOSDebugger::new()?;
/// debugger.attach(pid)?;
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId(pub u32);

impl From<u32> for ProcessId
{
    fn from(pid: u32) -> Self
    {
        ProcessId(pid)
    }
}

impl From<ProcessId> for u32
{
    fn from(pid: ProcessId) -> Self
    {
        pid.0
    }
}

/// Thread identifier
///
/// A thread identifier uniquely identifies a thread within a process. The exact
/// representation is platform-specific:
///
/// - **macOS**: Mach thread port (`thread_act_t`), which is a `mach_port_t`
/// - **Linux**: Thread ID (TID) from the kernel
/// - **Windows**: Thread handle or thread ID
///
/// We store it as a `u64` to provide a platform-agnostic interface. Platform-specific
/// implementations convert between their native types and `ThreadId`.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::types::ThreadId;
/// use ferros_core::Debugger;
///
/// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
/// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
/// let threads = debugger.threads()?;
/// if let Some(thread) = threads.first() {
///     debugger.set_active_thread(*thread)?;
/// }
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

impl ThreadId
{
    /// Get the raw `u64` representation of the thread identifier
    ///
    /// This returns the underlying platform-specific thread identifier as a `u64`.
    /// The exact meaning depends on the platform:
    ///
    /// - **macOS**: Mach thread port value
    /// - **Linux**: Thread ID (TID)
    /// - **Windows**: Thread handle or thread ID
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::ThreadId;
    ///
    /// let thread = ThreadId::from(12345);
    /// assert_eq!(thread.raw(), 12345);
    /// ```
    pub fn raw(&self) -> u64
    {
        self.0
    }
}

impl From<u64> for ThreadId
{
    fn from(value: u64) -> Self
    {
        Self(value)
    }
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

/// Reason why the debugger is currently stopped (if at all)
///
/// This enum describes why a debugged process is currently stopped. The process
/// can be stopped for various reasons: explicit suspension, signals, breakpoints,
/// or because it has exited.
///
/// ## State Transitions
///
/// - `Running` → `Suspended`: Process was explicitly suspended via `suspend()`
/// - `Running` → `Signal(n)`: Process received a signal (e.g., SIGSTOP, SIGINT)
/// - `Running` → `Breakpoint(addr)`: Process hit a breakpoint at `addr`
/// - `Running` → `Exited(code)`: Process exited with exit code `code`
/// - `Suspended` → `Running`: Process was resumed via `resume()`
///
/// ## Platform-Specific Behavior
///
/// - **macOS**: Uses `task_suspend()`/`task_resume()` for suspension
/// - **Linux**: Uses `ptrace(PTRACE_CONT)`/`ptrace(PTRACE_STOP)` for control
/// - **Windows**: Uses `SuspendThread()`/`ResumeThread()` for thread control
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::types::StopReason;
/// use ferros_core::Debugger;
///
/// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
/// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
/// match debugger.stop_reason() {
///     StopReason::Running => println!("Process is running"),
///     StopReason::Suspended => println!("Process is suspended"),
///     StopReason::Signal(sig) => println!("Stopped by signal: {}", sig),
///     StopReason::Breakpoint(addr) => println!("Hit breakpoint at 0x{:x}", addr),
///     StopReason::Exited(code) => println!("Process exited with code: {}", code),
///     StopReason::Unknown => println!("Stopped for unknown reason"),
/// }
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason
{
    /// Process is currently running (not stopped)
    Running,
    /// Process/task has been explicitly suspended
    ///
    /// This occurs when `suspend()` is called. The process can be resumed
    /// by calling `resume()`.
    Suspended,
    /// Stopped because a specific signal was delivered
    ///
    /// The `i32` value is the signal number (e.g., SIGSTOP = 19, SIGINT = 2).
    /// Common signals that stop processes:
    /// - `SIGSTOP` (19): Stop signal (cannot be caught or ignored)
    /// - `SIGTSTP` (20): Terminal stop signal
    /// - `SIGINT` (2): Interrupt signal (Ctrl+C)
    ///
    /// See: [signal(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/signal.3.html)
    Signal(i32),
    /// Hit a breakpoint at the provided address
    ///
    /// The `u64` value is the memory address where the breakpoint was hit.
    /// This is set when the process executes an instruction at a breakpoint location.
    Breakpoint(u64),
    /// Process exited with status code
    ///
    /// The `i32` value is the exit code (0 typically means success, non-zero means error).
    /// Once a process has exited, it cannot be resumed or debugged further.
    Exited(i32),
    /// Unknown/other reason
    ///
    /// The process is stopped for a reason that doesn't fit into the other categories.
    /// This may occur on some platforms or in edge cases.
    Unknown,
}

/// Strongly typed memory address
///
/// This wrapper around `u64` provides type safety when working with memory
/// addresses. It prevents accidentally mixing addresses with other `u64` values
/// (like sizes, counts, or other numeric types).
///
/// ## Why use a newtype?
///
/// - **Type safety**: Prevents accidentally passing a size where an address is expected
/// - **Self-documenting**: Makes it clear that a value represents a memory address
/// - **Future extensibility**: Can add address validation or methods later
///
/// ## Address Space
///
/// On 64-bit systems, addresses are 64-bit values. However, not all 64-bit values
/// are valid addresses. The actual addressable space depends on the platform:
///
/// - **macOS/Linux**: Typically 48-bit virtual addresses (can be extended to 57-bit)
/// - **Windows**: 48-bit virtual addresses
///
/// ## Example
///
/// ```rust
/// use ferros_core::types::Address;
///
/// let addr = Address::from(0x1000);
/// let next_addr = addr + 0x100; // Add offset
/// assert_eq!(next_addr.value(), 0x1100);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(u64);

impl Address
{
    /// The null address (0x0)
    ///
    /// This is typically an invalid address on most systems, but can be used
    /// as a sentinel value or for initialization.
    pub const ZERO: Self = Address(0);

    /// Create a new address from a `u64` value
    ///
    /// This is equivalent to `Address::from(value)` but can be used in const contexts.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// const STACK_BASE: Address = Address::new(0x7fff00000000);
    /// ```
    pub const fn new(value: u64) -> Self
    {
        Address(value)
    }

    /// Get the raw `u64` value of this address
    ///
    /// This returns the underlying address value. Use this when you need to pass
    /// the address to platform-specific APIs that expect a `u64`.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.value(), 0x1000);
    /// ```
    pub const fn value(self) -> u64
    {
        self.0
    }

    /// Add an offset to this address, checking for overflow
    ///
    /// Returns `Some(new_address)` if the addition doesn't overflow, or `None` if it does.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.checked_add(0x100), Some(Address::from(0x1100)));
    /// assert_eq!(addr.checked_add(u64::MAX), None); // Overflow
    /// ```
    pub fn checked_add(self, offset: u64) -> Option<Self>
    {
        self.0.checked_add(offset).map(Address)
    }

    /// Subtract an offset from this address, checking for underflow
    ///
    /// Returns `Some(new_address)` if the subtraction doesn't underflow, or `None` if it does.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.checked_sub(0x100), Some(Address::from(0xf00)));
    /// assert_eq!(addr.checked_sub(u64::MAX), None); // Underflow
    /// ```
    pub fn checked_sub(self, offset: u64) -> Option<Self>
    {
        self.0.checked_sub(offset).map(Address)
    }

    /// Add an offset to this address, saturating at the maximum value
    ///
    /// If the addition would overflow, returns `Address::new(u64::MAX)` instead.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Address;
    ///
    /// let addr = Address::from(0x1000);
    /// assert_eq!(addr.saturating_add(0x100), Address::from(0x1100));
    /// assert_eq!(addr.saturating_add(u64::MAX), Address::new(u64::MAX)); // Saturates
    /// ```
    pub fn saturating_add(self, offset: u64) -> Self
    {
        Address(self.0.saturating_add(offset))
    }
}

impl From<u64> for Address
{
    fn from(value: u64) -> Self
    {
        Address(value)
    }
}

impl From<Address> for u64
{
    fn from(address: Address) -> Self
    {
        address.0
    }
}

impl fmt::Display for Address
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        write!(f, "0x{:016x}", self.0)
    }
}

impl Add<u64> for Address
{
    type Output = Address;

    fn add(self, rhs: u64) -> Self::Output
    {
        Address(self.0.wrapping_add(rhs))
    }
}

impl Sub<u64> for Address
{
    type Output = Address;

    fn sub(self, rhs: u64) -> Self::Output
    {
        Address(self.0.wrapping_sub(rhs))
    }
}

/// Identifier for memory regions
///
/// This is a stable identifier for a memory region within a process. It's used
/// to track and reference memory regions across operations. The ID is assigned
/// sequentially when regions are enumerated (0, 1, 2, ...).
///
/// ## Stability
///
/// Memory region IDs are stable within a single enumeration session, but may
/// change if the process's memory layout changes (e.g., after `malloc()` or `mmap()`).
/// You should refresh the memory region list if you need up-to-date information.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::types::MemoryRegionId;
/// use ferros_core::Debugger;
///
/// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
/// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
/// let regions = debugger.get_memory_regions()?;
/// for region in regions {
///     println!(
///         "Region {}: {}-{}",
///         region.id.value(),
///         region.start,
///         region.end
///     );
/// }
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryRegionId(pub usize);

impl MemoryRegionId
{
    /// Get the raw `usize` value of this memory region identifier
    ///
    /// This returns the underlying ID value. Use this when you need to compare
    /// or store the ID as a number.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::MemoryRegionId;
    ///
    /// let id = MemoryRegionId(42);
    /// assert_eq!(id.value(), 42);
    /// ```
    pub fn value(self) -> usize
    {
        self.0
    }
}

/// Memory region in a process
///
/// Represents a contiguous region of memory in the target process,
/// such as the stack, heap, or code segments. Each region has a start
/// address, end address, and permission flags that determine what
/// operations are allowed on that memory.
///
/// ## Examples
///
/// ```
/// use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
///
/// // A readable and executable code segment
/// let code_segment = MemoryRegion::new(
///     MemoryRegionId(0),
///     Address::from(0x1000),
///     Address::from(0x2000),
///     "rx".to_string(),
///     Some("/usr/bin/example".to_string()),
/// );
///
/// // A readable and writable heap region
/// let heap = MemoryRegion::new(
///     MemoryRegionId(1),
///     Address::from(0x2000),
///     Address::from(0x3000),
///     "rw".to_string(),
///     Some("[heap]".to_string()),
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion
{
    /// Stable identifier for the region.
    pub id: MemoryRegionId,
    /// Start address of the memory region (inclusive)
    ///
    /// This is the virtual address where the region begins in the
    /// target process's address space.
    pub start: Address,

    /// End address of the memory region (exclusive)
    ///
    /// This is the virtual address where the region ends. The region
    /// includes addresses from `start` (inclusive) to `end` (exclusive).
    /// The size of the region is `end - start`.
    pub end: Address,

    /// Memory permissions as a string
    ///
    /// Contains characters indicating allowed operations:
    /// - `r`: Read permission
    /// - `w`: Write permission
    /// - `x`: Execute permission
    ///
    /// Examples: `"rwx"` (read, write, execute), `"r-x"` (read, execute),
    /// `"rw-"` (read, write), `"r--"` (read-only).
    pub permissions: String,

    /// Optional name/description of the region
    ///
    /// On Linux, this might be `"[heap]"`, `"[stack]"`, or a file path
    /// like `"/usr/bin/example"`. On macOS, this is typically `None`
    /// as `mach_vm_region()` doesn't easily provide region names.
    pub name: Option<String>,
}

/// CPU architecture of the debug target
///
/// This enum represents the CPU architecture of the process being debugged.
/// Different architectures have different register layouts, instruction sets,
/// and debugging APIs.
///
/// ## Supported Architectures
///
/// - **Arm64**: 64-bit ARM (Apple Silicon M1, M2, M3, M4, etc.)
/// - **X86_64**: 64-bit x86 (Intel/AMD processors)
/// - **Unknown**: Other architectures (not yet supported)
///
/// ## Architecture Detection
///
/// The architecture is typically detected when attaching to a process. On macOS,
/// we use the architecture of the currently running debugger binary as a hint,
/// but the actual architecture may differ if debugging a cross-architecture process.
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::types::Architecture;
/// use ferros_core::Debugger;
///
/// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
/// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
/// match debugger.architecture() {
///     Architecture::Arm64 => println!("Debugging ARM64 process"),
///     Architecture::X86_64 => println!("Debugging x86-64 process"),
///     Architecture::Unknown(name) => println!("Unknown architecture: {}", name),
/// }
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Architecture
{
    /// 64-bit ARM (Apple Silicon)
    ///
    /// This architecture is used by Apple Silicon Macs (M1, M2, M3, M4, etc.).
    /// ARM64 has 31 general-purpose registers (X0-X30) plus special registers
    /// like SP (stack pointer) and PC (program counter).
    ///
    /// See: [ARM64 Architecture Reference Manual](https://developer.arm.com/documentation/ddi0487/latest)
    Arm64,
    /// 64-bit x86 (Intel/AMD)
    ///
    /// This architecture is used by Intel and AMD processors. x86-64 has 16
    /// general-purpose registers (RAX, RBX, RCX, RDX, RSI, RDI, R8-R15) plus
    /// special registers like RSP (stack pointer) and RIP (instruction pointer).
    ///
    /// See: [Intel 64 and IA-32 Architectures Software Developer's Manual](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
    X86_64,
    /// Any other architecture (or unknown)
    ///
    /// The `&'static str` contains the architecture name (e.g., "riscv64", "powerpc64").
    /// These architectures are not yet supported by the debugger.
    Unknown(&'static str),
}

impl Architecture
{
    /// Get the architecture of the currently running debugger binary
    ///
    /// This uses Rust's `#[cfg(target_arch = "...")]` to determine the architecture
    /// at compile time. It's useful as a default when creating a debugger instance,
    /// though the actual target process may have a different architecture.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::Architecture;
    ///
    /// let arch = Architecture::current();
    /// // On Apple Silicon: Architecture::Arm64
    /// // On Intel Mac: Architecture::X86_64
    /// ```
    pub const fn current() -> Self
    {
        #[cfg(target_arch = "aarch64")]
        {
            Architecture::Arm64
        }

        #[cfg(target_arch = "x86_64")]
        {
            Architecture::X86_64
        }

        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            Architecture::Unknown(std::env::consts::ARCH)
        }
    }
}

impl fmt::Display for Architecture
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        match self {
            Architecture::Arm64 => write!(f, "arm64"),
            Architecture::X86_64 => write!(f, "x86_64"),
            Architecture::Unknown(name) => write!(f, "{name}"),
        }
    }
}

impl MemoryRegion
{
    /// Create a new memory region
    ///
    /// ## Parameters
    ///
    /// - `start`: Start address of the region (inclusive)
    /// - `end`: End address of the region (exclusive)
    /// - `permissions`: Permission string (e.g., `"rwx"`, `"r-x"`, `"rw-"`)
    /// - `name`: Optional name/description of the region
    ///
    /// ## Panics
    ///
    /// This function does not validate that `end > start`. If `end <= start`,
    /// `size()` will return 0.
    pub fn new(id: MemoryRegionId, start: Address, end: Address, permissions: String, name: Option<String>) -> Self
    {
        Self {
            id,
            start,
            end,
            permissions,
            name,
        }
    }

    /// Get the size of the memory region in bytes
    ///
    /// Returns `end - start`, or 0 if `end <= start` (using saturating subtraction
    /// to prevent underflow).
    ///
    /// ## Example
    ///
    /// ```
    /// use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
    ///
    /// let region = MemoryRegion::new(
    ///     MemoryRegionId(0),
    ///     Address::from(0x1000),
    ///     Address::from(0x2000),
    ///     "rwx".to_string(),
    ///     None,
    /// );
    /// assert_eq!(region.size(), 0x1000); // 4096 bytes
    /// ```
    pub fn size(&self) -> u64
    {
        self.end.value().saturating_sub(self.start.value())
    }

    /// Check if the region is readable
    ///
    /// Returns `true` if the permissions string contains `'r'`.
    ///
    /// ## Example
    ///
    /// ```
    /// use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
    ///
    /// let region = MemoryRegion::new(
    ///     MemoryRegionId(0),
    ///     Address::from(0x1000),
    ///     Address::from(0x2000),
    ///     "r-x".to_string(),
    ///     None,
    /// );
    /// assert!(region.is_readable());
    /// ```
    pub fn is_readable(&self) -> bool
    {
        self.permissions.contains('r')
    }

    /// Check if the region is writable
    ///
    /// Returns `true` if the permissions string contains `'w'`.
    ///
    /// ## Example
    ///
    /// ```
    /// use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
    ///
    /// let region = MemoryRegion::new(
    ///     MemoryRegionId(0),
    ///     Address::from(0x1000),
    ///     Address::from(0x2000),
    ///     "rw-".to_string(),
    ///     None,
    /// );
    /// assert!(region.is_writable());
    /// ```
    pub fn is_writable(&self) -> bool
    {
        self.permissions.contains('w')
    }

    /// Check if the region is executable
    ///
    /// Returns `true` if the permissions string contains `'x'`.
    ///
    /// ## Example
    ///
    /// ```
    /// use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
    ///
    /// let region = MemoryRegion::new(
    ///     MemoryRegionId(0),
    ///     Address::from(0x1000),
    ///     Address::from(0x2000),
    ///     "r-x".to_string(),
    ///     None,
    /// );
    /// assert!(region.is_executable());
    /// ```
    pub fn is_executable(&self) -> bool
    {
        self.permissions.contains('x')
    }

    /// Check if an address lies within this memory region
    ///
    /// Returns `true` if the address is greater than or equal to `start` and
    /// less than `end` (i.e., within the region's address range).
    ///
    /// ## Example
    ///
    /// ```rust
    /// use ferros_core::types::{Address, MemoryRegion, MemoryRegionId};
    ///
    /// let region = MemoryRegion::new(
    ///     MemoryRegionId(0),
    ///     Address::from(0x1000),
    ///     Address::from(0x2000),
    ///     "rwx".to_string(),
    ///     None,
    /// );
    ///
    /// assert!(region.contains(Address::from(0x1000))); // Start (inclusive)
    /// assert!(region.contains(Address::from(0x1500))); // Middle
    /// assert!(!region.contains(Address::from(0x2000))); // End (exclusive)
    /// assert!(!region.contains(Address::from(0x500))); // Before start
    /// ```
    pub fn contains(&self, address: Address) -> bool
    {
        address >= self.start && address < self.end
    }
}
