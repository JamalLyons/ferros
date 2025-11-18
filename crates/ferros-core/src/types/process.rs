//! Process, thread, and memory region types.

use std::fmt;

use super::Address;

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

    /// Size of a pointer in bytes for this architecture.
    #[must_use]
    pub const fn pointer_size_bytes(self) -> u8
    {
        match self {
            Architecture::Arm64 | Architecture::X86_64 => 8,
            Architecture::Unknown(_) => 8,
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
