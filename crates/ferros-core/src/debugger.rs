//! # Debugger Trait
//!
//! The main interface for platform-specific debugger implementations.
//!
//! This trait defines what a debugger can do, regardless of the underlying
//! platform (macOS, Linux, Windows). Each platform implements this trait
//! using its own system APIs:
//!
//! - **macOS**: Uses Mach APIs (`task_for_pid`, `thread_get_state`)
//! - **Linux**: Will use `ptrace` system call
//! - **Windows**: Will use Windows Debug API
//!
//! ## Why use a trait?
//!
//! Traits allow us to:
//! - Write platform-agnostic code that works on all platforms
//! - Swap implementations easily (e.g., for testing)
//! - Hide platform-specific details behind a clean interface
//!
//! ## Design Philosophy
//!
//! The trait methods are designed to be:
//! - **Simple**: Each method does one thing
//! - **Safe**: Wrap unsafe system calls in safe abstractions
//! - **Explicit**: Clear about what they do and when they can fail

use crate::error::Result;
use crate::types::{ProcessId, Registers};

/// Main debugger interface
///
/// This trait defines the operations a debugger can perform on a process.
/// All platform-specific implementations (macOS, Linux, Windows) must
/// implement these methods.
///
/// ## Lifecycle
///
/// 1. Create a debugger: `MacOSDebugger::new()`
/// 2. Attach to a process: `attach(pid)`
/// 3. Inspect/manipulate: `read_registers()`, `write_registers()`, etc.
/// 4. Detach: `detach()`
///
/// ## Thread Safety
///
/// The debugger is **not** thread-safe. Each debugger instance should be
/// used from a single thread. If you need multi-threaded access, wrap it
/// in a `Mutex` or use channels to communicate with a debugger thread.
pub trait Debugger
{
    /// Attach to a running process
    ///
    /// This establishes a connection to the target process, allowing you
    /// to inspect and control it. The process continues running normally
    /// after attachment (unlike some debuggers that stop it immediately).
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Calls `task_for_pid()` to get a Mach task port
    /// - **Linux**: Will call `ptrace(PTRACE_ATTACH, pid)`
    /// - **Windows**: Will call `DebugActiveProcess(pid)`
    ///
    /// ## Errors
    ///
    /// - `ProcessNotFound`: The PID doesn't exist
    /// - `PermissionDenied`: Insufficient permissions (need sudo/entitlements)
    /// - `AttachFailed`: Other attachment failures
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::ProcessId;
    /// use ferros_core::Debugger;
    ///
    /// let mut debugger = MacOSDebugger::new()?;
    /// debugger.attach(ProcessId::from(12345))?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn attach(&mut self, pid: ProcessId) -> Result<()>;

    /// Detach from the process
    ///
    /// Releases the connection to the target process. After detaching,
    /// you can no longer inspect or control the process.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Releases the Mach task port (no explicit detach needed)
    /// - **Linux**: Will call `ptrace(PTRACE_DETACH, pid)`
    /// - **Windows**: Will call `DebugActiveProcessStop(pid)`
    ///
    /// ## Note
    ///
    /// On macOS, detaching doesn't actually do anything - the task port
    /// is automatically released when the debugger struct is dropped.
    /// But we provide this method for consistency across platforms.
    fn detach(&mut self) -> Result<()>;

    /// Read registers from the attached process
    ///
    /// Reads the current values of all CPU registers from the target process.
    /// This gives you a snapshot of the process's execution state.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Calls `thread_get_state()` with `ARM_THREAD_STATE64` or `X86_THREAD_STATE64`
    /// - **Linux**: Will call `ptrace(PTRACE_GETREGS, pid)`
    /// - **Windows**: Will call `GetThreadContext()`
    ///
    /// ## Errors
    ///
    /// - `AttachFailed`: Not attached to a process
    /// - `ReadRegistersFailed`: Failed to read thread state
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// let regs = debugger.read_registers()?;
    /// println!("Program counter: 0x{:x}", regs.pc);
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn read_registers(&self) -> Result<Registers>;

    /// Write registers to the attached process
    ///
    /// Modifies the register values in the target process. This can be used
    /// to change the program's execution flow (e.g., jump to a different address).
    ///
    /// ## ⚠️ Warning
    ///
    /// Modifying registers can crash the process or cause undefined behavior.
    /// Only do this if you know what you're doing!
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Will call `thread_set_state()` (not yet implemented)
    /// - **Linux**: Will call `ptrace(PTRACE_SETREGS, pid)`
    /// - **Windows**: Will call `SetThreadContext()`
    ///
    /// ## Status
    ///
    /// Not yet implemented - returns `InvalidArgument` error.
    fn write_registers(&mut self, regs: &Registers) -> Result<()>;

    // Future methods (commented out until implemented):
    //
    // /// Read memory from the target process
    // /// Uses platform-specific APIs: vm_read (macOS), process_vm_readv (Linux), ReadProcessMemory (Windows)
    // fn read_memory(&self, addr: u64, len: usize) -> Result<Vec<u8>>;
    //
    // /// Write memory to the target process
    // /// Uses platform-specific APIs: vm_write (macOS), process_vm_writev (Linux), WriteProcessMemory (Windows)
    // fn write_memory(&mut self, addr: u64, data: &[u8]) -> Result<()>;
    //
    // /// Set a breakpoint at the given address
    // /// On x86-64/ARM64, this typically involves replacing the instruction with INT3 (x86) or BRK (ARM)
    // fn set_breakpoint(&mut self, addr: u64) -> Result<()>;
    //
    // /// Continue execution of the target process
    // /// Resumes execution after being stopped (by breakpoint, signal, etc.)
    // fn continue_execution(&mut self) -> Result<()>;
    //
    // /// Single-step execution (execute one instruction)
    // /// Uses hardware single-step support: TF flag (x86) or SS bit (ARM)
    // fn single_step(&mut self) -> Result<()>;
}

/// Factory function to create a platform-specific debugger
///
/// This function automatically creates the correct debugger implementation
/// for the current platform. It uses conditional compilation (`#[cfg]`)
/// to select the right implementation at compile time.
///
/// ## Why a factory function?
///
/// - **Convenience**: Users don't need to know which debugger type to use
/// - **Platform abstraction**: Same code works on all platforms
/// - **Type erasure**: Returns `Box<dyn Debugger>` so you can store it generically
///
/// ## Example
///
/// ```rust,no_run
/// use ferros_core::debugger::create_debugger;
/// use ferros_core::types::ProcessId;
/// use ferros_core::Debugger;
///
/// let mut debugger = create_debugger()?;
/// debugger.attach(ProcessId::from(12345))?;
/// # Ok::<(), ferros_core::error::DebuggerError>(())
/// ```
///
/// ## Platform Support
///
/// - ✅ macOS: Returns `MacOSDebugger`
/// - ⏳ Linux: Will return `LinuxDebugger` (future)
/// - ⏳ Windows: Will return `WindowsDebugger` (future)
pub fn create_debugger() -> Result<Box<dyn Debugger>>
{
    #[cfg(target_os = "macos")]
    return Ok(Box::new(crate::platform::macos::MacOSDebugger::new()?));

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(crate::error::DebuggerError::InvalidArgument(
        "Unsupported platform".to_string(),
    ));
}
