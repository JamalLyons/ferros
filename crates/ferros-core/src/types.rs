//! # Types
//!
//! Platform-agnostic types used throughout the debugger.
//!
//! These types abstract away platform-specific details, allowing the rest of
//! the debugger to work with concepts like "process ID" and "registers" without
//! knowing whether we're on macOS, Linux, or Windows.

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

/// Platform-agnostic register representation
///
/// Registers are small, fast storage locations inside the CPU. They hold
/// values that the CPU is currently working with. Different architectures
/// have different registers, but most have these common ones:
///
/// - **PC (Program Counter)**: Points to the next instruction to execute
/// - **SP (Stack Pointer)**: Points to the top of the stack
/// - **FP (Frame Pointer)**: Points to the current stack frame
/// - **General-purpose registers**: Hold data values (X0-X31 on ARM64, RAX-R15 on x86-64)
/// - **Status register**: Contains flags like "was the last operation zero?"
///
/// ## Why this structure?
///
/// We store registers in a platform-agnostic format so that:
/// - The rest of the debugger doesn't need to know about ARM64 vs x86-64
/// - We can display registers consistently across platforms
/// - We can implement features like "show all registers" without platform-specific code
///
/// ## Register Layout by Architecture
///
/// ### ARM64 (Apple Silicon)
/// - PC: Program Counter (X32 in ARM_THREAD_STATE64)
/// - SP: Stack Pointer (X31)
/// - FP: Frame Pointer (X29)
/// - General: X0-X15 (first 16 general-purpose registers)
/// - Status: CPSR (Current Program Status Register)
///
/// ### x86-64 (Intel Mac)
/// - PC: RIP (Instruction Pointer)
/// - SP: RSP (Stack Pointer)
/// - FP: RBP (Base Pointer)
/// - General: RAX, RBX, RCX, RDX, RSI, RDI, R8-R15
/// - Status: RFLAGS
///
/// ## References
///
/// - [ARM64 Register Layout](https://developer.arm.com/documentation/102374/0101/Registers-in-AArch64---general-purpose-registers)
/// - [x86-64 Register Layout](https://en.wikipedia.org/wiki/X86-64#Registers)
/// - [ARM CPSR Register](https://developer.arm.com/documentation/dui0801/a/A32-and-T32-Instructions/CPSR)
#[derive(Debug, Clone)]
pub struct Registers
{
    /// Program counter / instruction pointer
    ///
    /// This register holds the memory address of the next instruction to execute.
    /// When you "step" through code, you're incrementing this register.
    ///
    /// - ARM64: PC register (index 32 in ARM_THREAD_STATE64)
    /// - x86-64: RIP register
    pub pc: u64,

    /// Stack pointer
    ///
    /// Points to the top of the stack. The stack grows downward in memory,
    /// so SP typically decreases as you push values onto it.
    ///
    /// - ARM64: SP register (index 31)
    /// - x86-64: RSP register
    pub sp: u64,

    /// Frame pointer / base pointer
    ///
    /// Points to the current stack frame. Used to access local variables
    /// and function parameters. Each function call creates a new stack frame.
    ///
    /// - ARM64: FP/X29 register (index 29)
    /// - x86-64: RBP register
    pub fp: u64,

    /// General-purpose registers (architecture-specific)
    ///
    /// These registers hold data values that the program is working with.
    /// The number and names vary by architecture:
    ///
    /// - ARM64: X0-X31 (we typically read X0-X15)
    /// - x86-64: RAX, RBX, RCX, RDX, RSI, RDI, R8-R15
    ///
    /// The first few registers (X0-X7 on ARM64, RAX-RDX on x86-64) are
    /// often used for function arguments and return values.
    pub general: Vec<u64>,

    /// Architecture-specific status/flag register
    ///
    /// Contains flags that indicate the result of previous operations:
    /// - Zero flag: Was the result zero?
    /// - Negative flag: Was the result negative?
    /// - Carry flag: Did an addition overflow?
    ///
    /// - ARM64: CPSR (Current Program Status Register)
    /// - x86-64: RFLAGS
    pub status: u64,
}

impl Registers
{
    /// Create a new, empty Registers struct
    ///
    /// All values are initialized to zero. This is useful when you need
    /// a placeholder before reading actual register values from a process.
    pub fn new() -> Self
    {
        Self {
            pc: 0,
            sp: 0,
            fp: 0,
            general: Vec::new(),
            status: 0,
        }
    }
}

impl Default for Registers
{
    fn default() -> Self
    {
        Self::new()
    }
}
