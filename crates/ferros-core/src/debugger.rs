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

use std::fs::File;

use crate::error::Result;
use crate::types::{Address, Architecture, ProcessId, Registers, StopReason, ThreadId};

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
    /// Configure whether stdout/stderr from launched processes should be captured.
    ///
    /// The default implementation does nothing. Platform-specific implementations
    /// can override this to enable or disable stdio redirection before calling
    /// [`Debugger::launch`].
    fn set_capture_process_output(&mut self, _capture: bool) {}

    /// Take ownership of the captured stdout stream for the most recently launched process.
    ///
    /// Returns `None` if output capture is disabled or unsupported.
    fn take_process_stdout(&mut self) -> Option<File>
    {
        None
    }

    /// Take ownership of the captured stderr stream for the most recently launched process.
    ///
    /// Returns `None` if output capture is disabled or unsupported.
    fn take_process_stderr(&mut self) -> Option<File>
    {
        None
    }

    /// Launch a new process under debugger control
    ///
    /// Spawns a new process from the given executable path and arguments, and
    /// immediately attaches the debugger to it. The process starts in a suspended
    /// state, allowing you to set breakpoints before it begins execution.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Uses `posix_spawn()` with `POSIX_SPAWN_START_SUSPENDED` flag,
    ///   then calls `attach()` to get the task port. This avoids permission issues
    ///   that can occur when attaching to already-running processes.
    /// - **Linux**: Will use `fork()` + `execve()` with `PTRACE_TRACEME`
    /// - **Windows**: Will use `CreateProcess()` with `DEBUG_PROCESS` flag
    ///
    /// ## Advantages over `attach()`
    ///
    /// - Avoids permission issues (no need for sudo or entitlements when launching)
    /// - Process starts suspended, so you can set breakpoints before execution
    /// - More reliable than attaching to a running process
    ///
    /// ## Parameters
    ///
    /// - `program`: Path to the executable to launch
    /// - `args`: Command-line arguments (first argument should be the program name)
    ///
    /// ## Errors
    ///
    /// - `InvalidArgument`: Invalid program path or arguments
    /// - `AttachFailed`: Failed to spawn process or attach to it
    /// - `Io`: I/O error (e.g., file not found, permission denied)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::Debugger;
    ///
    /// let mut debugger = MacOSDebugger::new()?;
    /// let pid = debugger.launch("/usr/bin/echo", &["echo", "Hello, world!"])?;
    /// // Process is now suspended and ready for debugging
    /// println!("Launched process with PID: {}", pid.0);
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn launch(&mut self, program: &str, args: &[&str]) -> Result<ProcessId>;

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
    /// - `AttachFailed`: Other attachment failures (e.g., failed to enumerate threads)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::platform::macos::MacOSDebugger;
    /// use ferros_core::types::{Address, ProcessId};
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
    /// - `NotAttached`: Not attached to a process
    /// - `ReadRegistersFailed`: Failed to read thread state
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// let regs = debugger.read_registers()?;
    /// println!("Program counter: {}", regs.pc);
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

    /// Read memory from the target process
    ///
    /// Reads `len` bytes starting at the given address from the attached process.
    /// Returns a vector containing the read bytes.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Uses `vm_read()` to read memory from the Mach task
    /// - **Linux**: Uses `ptrace(PTRACE_PEEKDATA)` in a loop to read word-aligned data
    /// - **Windows**: Uses `ReadProcessMemory()`
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Invalid memory address or length
    /// - `Io`: Failed to read memory (e.g., invalid address, permission denied)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::{Address, Debugger};
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let data = debugger.read_memory(Address::from(0x1000), 16)?;
    /// println!("Read {} bytes: {:?}", data.len(), data);
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn read_memory(&self, addr: Address, len: usize) -> Result<Vec<u8>>;

    /// Write memory to the target process
    ///
    /// Writes `data` bytes starting at the given address in the attached process.
    /// Returns the number of bytes written.
    ///
    /// ## ⚠️ Warning
    ///
    /// Writing to memory can crash the process or cause undefined behavior.
    /// Only write to writable memory regions (e.g., stack, heap).
    /// Writing to code segments may corrupt the program.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Uses `vm_write()` to write memory to the Mach task
    /// - **Linux**: Uses `ptrace(PTRACE_POKEDATA)` in a loop to write word-aligned data
    /// - **Windows**: Uses `WriteProcessMemory()`
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: Invalid memory address or data length
    /// - `Io`: Failed to write memory (e.g., read-only memory, permission denied)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::types::Address;
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let data = vec![0x41, 0x42, 0x43, 0x44];
    /// debugger.write_memory(Address::from(0x1000), &data)?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn write_memory(&mut self, addr: Address, data: &[u8]) -> Result<usize>;

    /// Get memory regions for the attached process
    ///
    /// Returns a list of all memory regions (segments) in the target process.
    /// This includes code segments, data segments, stack, heap, and mapped files.
    ///
    /// Each region contains information about its address range, permissions
    /// (read/write/execute), and optionally a name or description.
    ///
    /// ## Platform-specific behavior
    ///
    /// - **macOS**: Uses `mach_vm_region()` to enumerate memory regions
    /// - **Linux**: Parses `/proc/[pid]/maps` file
    /// - **Windows**: Uses `VirtualQueryEx()` to enumerate memory regions
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `Io`: Failed to read memory map information
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let regions = debugger.get_memory_regions()?;
    /// for region in regions {
    ///     println!(
    ///         "{}-{} {} {:?}",
    ///         region.start, region.end, region.permissions, region.name
    ///     );
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn get_memory_regions(&self) -> Result<Vec<crate::types::MemoryRegion>>;

    /// Get the CPU architecture of the debug target
    ///
    /// Returns the architecture of the process being debugged. This is typically
    /// determined when attaching to the process, though some platforms may detect
    /// it earlier.
    ///
    /// ## Platform-Specific Behavior
    ///
    /// - **macOS**: Uses the architecture of the debugger binary as a hint, but
    ///   the actual target process architecture may differ (e.g., debugging x86-64
    ///   process from ARM64 debugger)
    /// - **Linux**: Detected from `/proc/[pid]/exe` or ELF headers
    /// - **Windows**: Detected from PE headers
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
    fn architecture(&self) -> Architecture;

    /// Check whether the debugger is currently attached to a process
    ///
    /// Returns `true` if `attach()` has been called successfully and the debugger
    /// is still connected to the target process. Returns `false` if:
    /// - The debugger was never attached
    /// - The debugger was detached via `detach()`
    /// - The target process has exited
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// if !debugger.is_attached() {
    ///     debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn is_attached(&self) -> bool;

    /// Check whether the debuggee is currently stopped/suspended
    ///
    /// Returns `true` if the target process is currently stopped (suspended,
    /// hit a breakpoint, received a signal, etc.). Returns `false` if the process
    /// is running.
    ///
    /// ## Relationship to `stop_reason()`
    ///
    /// - `is_stopped() == true` → `stop_reason()` is not `StopReason::Running`
    /// - `is_stopped() == false` → `stop_reason()` is `StopReason::Running`
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// if debugger.is_stopped() {
    ///     println!("Process is stopped, can inspect registers/memory");
    /// } else {
    ///     println!("Process is running");
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn is_stopped(&self) -> bool;

    /// Get the most recent stop reason
    ///
    /// Returns the reason why the process is currently stopped (if at all).
    /// This can be used to determine what action to take next:
    ///
    /// - `StopReason::Running`: Process is running (not stopped)
    /// - `StopReason::Suspended`: Process was explicitly suspended
    /// - `StopReason::Signal(n)`: Process received a signal
    /// - `StopReason::Breakpoint(addr)`: Process hit a breakpoint
    /// - `StopReason::Exited(code)`: Process has exited
    /// - `StopReason::Unknown`: Unknown reason
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
    ///     StopReason::Breakpoint(addr) => {
    ///         println!("Hit breakpoint at 0x{:x}", addr);
    ///         // Inspect registers, memory, etc.
    ///     }
    ///     StopReason::Signal(sig) => {
    ///         println!("Stopped by signal: {}", sig);
    ///     }
    ///     _ => {}
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn stop_reason(&self) -> StopReason;

    /// Suspend execution of the target process
    ///
    /// Stops the target process from executing. After calling this, the process
    /// will be in a stopped state and you can safely inspect its registers,
    /// memory, and other state without it changing.
    ///
    /// ## Platform-Specific Behavior
    ///
    /// - **macOS**: Calls `task_suspend()` to suspend the Mach task
    ///   - See: [task_suspend documentation](https://developer.apple.com/documentation/kernel/1402800-task_suspend)
    /// - **Linux**: Will use `ptrace(PTRACE_INTERRUPT)` or `kill(pid, SIGSTOP)`
    /// - **Windows**: Will use `SuspendThread()` for each thread
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `SuspendFailed`: Failed to suspend the process (e.g., process already exited)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// debugger.suspend()?;
    /// // Now safe to inspect process state
    /// let regs = debugger.read_registers()?;
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn suspend(&mut self) -> Result<()>;

    /// Resume execution after being stopped
    ///
    /// Resumes execution of the target process. The process will continue running
    /// from where it was stopped (unless registers were modified).
    ///
    /// ## Platform-Specific Behavior
    ///
    /// - **macOS**: Calls `task_resume()` to resume the Mach task
    ///   - See: [task_resume documentation](https://developer.apple.com/documentation/kernel/1402801-task_resume)
    /// - **Linux**: Will use `ptrace(PTRACE_CONT)` to continue execution
    /// - **Windows**: Will use `ResumeThread()` for each thread
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `ResumeFailed`: Failed to resume the process (e.g., process already exited)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// debugger.suspend()?;
    /// // ... inspect process state ...
    /// debugger.resume()?; // Continue execution
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn resume(&mut self) -> Result<()>;

    /// List all available threads in the target process
    ///
    /// Returns a vector of `ThreadId` values representing all threads currently
    /// in the target process. This list is cached and may become stale if threads
    /// are created or destroyed. Call `refresh_threads()` to update it.
    ///
    /// ## Platform-Specific Behavior
    ///
    /// - **macOS**: Returns thread ports from `task_threads()`
    ///   - See: [task_threads documentation](https://developer.apple.com/documentation/kernel/1402802-task_threads)
    /// - **Linux**: Will parse `/proc/[pid]/task/` directory
    /// - **Windows**: Will use `CreateToolhelp32Snapshot()` with `TH32CS_SNAPTHREAD`
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `AttachFailed`: Failed to enumerate threads
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let threads = debugger.threads()?;
    /// println!("Process has {} threads", threads.len());
    /// for thread in threads {
    ///     println!("Thread: {}", thread.raw());
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn threads(&self) -> Result<Vec<ThreadId>>;

    /// Get the currently selected active thread (if any)
    ///
    /// Returns `Some(thread_id)` if an active thread has been selected via
    /// `set_active_thread()`, or `None` if no thread is selected. The active
    /// thread is used for register operations (`read_registers()`, `write_registers()`).
    ///
    /// ## Default Behavior
    ///
    /// When attaching to a process, the first thread (typically the main thread)
    /// is automatically selected as the active thread.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// if let Some(thread) = debugger.active_thread() {
    ///     println!("Active thread: {}", thread.raw());
    /// } else {
    ///     println!("No active thread selected");
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn active_thread(&self) -> Option<ThreadId>;

    /// Select the active thread that subsequent operations should target
    ///
    /// Sets the active thread for register operations. After calling this, `read_registers()`
    /// and `write_registers()` will operate on the specified thread.
    ///
    /// ## Parameters
    ///
    /// - `thread`: The thread ID to make active. Must be a valid thread from `threads()`.
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `InvalidArgument`: The thread ID is not valid (not in the thread list)
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// let threads = debugger.threads()?;
    /// if let Some(thread) = threads.get(1) {
    ///     debugger.set_active_thread(*thread)?;
    ///     // Now read_registers() will read from thread 1
    ///     let regs = debugger.read_registers()?;
    /// }
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn set_active_thread(&mut self, thread: ThreadId) -> Result<()>;

    /// Refresh the thread list from the operating system
    ///
    /// Updates the cached thread list by querying the operating system for the
    /// current set of threads in the target process. This should be called if:
    /// - Threads may have been created or destroyed since the last call
    /// - You need up-to-date thread information
    ///
    /// ## Note
    ///
    /// The active thread is preserved if it still exists. If the active thread
    /// has exited, the first thread in the new list becomes the active thread.
    ///
    /// ## Platform-Specific Behavior
    ///
    /// - **macOS**: Calls `task_threads()` to refresh the thread list
    ///   - See: [task_threads documentation](https://developer.apple.com/documentation/kernel/1402802-task_threads)
    /// - **Linux**: Will re-read `/proc/[pid]/task/` directory
    /// - **Windows**: Will use `CreateToolhelp32Snapshot()` again
    ///
    /// ## Errors
    ///
    /// - `NotAttached`: Not attached to a process
    /// - `AttachFailed`: Failed to enumerate threads
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use ferros_core::Debugger;
    ///
    /// # let mut debugger = ferros_core::platform::macos::MacOSDebugger::new()?;
    /// # debugger.attach(ferros_core::types::ProcessId::from(12345))?;
    /// // Thread list may be stale
    /// debugger.refresh_threads()?; // Update thread list
    /// let threads = debugger.threads()?; // Now up-to-date
    /// # Ok::<(), ferros_core::error::DebuggerError>(())
    /// ```
    fn refresh_threads(&mut self) -> Result<()>;

    // Future methods (commented out until implemented):
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
    {
        Ok(Box::new(crate::platform::macos::MacOSDebugger::new()?))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(crate::error::DebuggerError::AttachFailed(format!(
            "Debugger not yet implemented for platform: {}",
            std::env::consts::OS
        )))
    }
}
