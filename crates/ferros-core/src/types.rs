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
/// use ferros_core::types::MemoryRegion;
///
/// // A readable and executable code segment
/// let code_segment = MemoryRegion::new(
///     0x1000,
///     0x2000,
///     "rx".to_string(),
///     Some("/usr/bin/example".to_string()),
/// );
///
/// // A readable and writable heap region
/// let heap = MemoryRegion::new(0x2000, 0x3000, "rw".to_string(), Some("[heap]".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion
{
    /// Start address of the memory region (inclusive)
    ///
    /// This is the virtual address where the region begins in the
    /// target process's address space.
    pub start: u64,

    /// End address of the memory region (exclusive)
    ///
    /// This is the virtual address where the region ends. The region
    /// includes addresses from `start` (inclusive) to `end` (exclusive).
    /// The size of the region is `end - start`.
    pub end: u64,

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
    pub fn new(start: u64, end: u64, permissions: String, name: Option<String>) -> Self
    {
        Self {
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
    /// use ferros_core::types::MemoryRegion;
    ///
    /// let region = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), None);
    /// assert_eq!(region.size(), 0x1000); // 4096 bytes
    /// ```
    pub fn size(&self) -> u64
    {
        self.end.saturating_sub(self.start)
    }

    /// Check if the region is readable
    ///
    /// Returns `true` if the permissions string contains `'r'`.
    ///
    /// ## Example
    ///
    /// ```
    /// use ferros_core::types::MemoryRegion;
    ///
    /// let region = MemoryRegion::new(0x1000, 0x2000, "r-x".to_string(), None);
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
    /// use ferros_core::types::MemoryRegion;
    ///
    /// let region = MemoryRegion::new(0x1000, 0x2000, "rw-".to_string(), None);
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
    /// use ferros_core::types::MemoryRegion;
    ///
    /// let region = MemoryRegion::new(0x1000, 0x2000, "r-x".to_string(), None);
    /// assert!(region.is_executable());
    /// ```
    pub fn is_executable(&self) -> bool
    {
        self.permissions.contains('x')
    }
}
