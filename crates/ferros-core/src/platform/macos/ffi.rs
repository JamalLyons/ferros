//! # macOS Mach API FFI Declarations
//!
//! This module contains all unsafe extern "C" function declarations for Mach APIs
//! that are not provided by the `mach2` crate. These functions require special
//! permissions (sudo or debugging entitlements) and are therefore not included
//! in the standard Mach bindings.
//!
//! ## Why Centralize These?
//!
//! - **Visibility**: All FFI declarations in one place for easy review
//! - **Documentation**: Centralized documentation of what each function does
//! - **Maintenance**: Easier to update when macOS APIs change
//! - **Safety**: Clear separation between safe Rust code and unsafe FFI
//!
//! ## Safety Notes
//!
//! All functions in this module are marked `unsafe` because they:
//! - Interact directly with the kernel
//! - Can crash the system if misused
//! - Require special permissions
//! - May have platform-specific behavior
//!
//! These functions are wrapped in safe abstractions in other modules.
//!
//! ## References
//!
//! - [Apple Mach Kernel Programming](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/Mach/Mach.html)
//! - [Mach System Calls](https://developer.apple.com/documentation/kernel)

// Allow doc comments in extern blocks - they're useful for developers even if rustdoc doesn't generate docs
#![allow(unused_doc_comments)]

use libc::{
    c_int, kern_return_t, mach_msg_type_number_t, mach_port_t, natural_t, thread_act_t, vm_address_t, vm_map_t, vm_offset_t,
    vm_size_t,
};

/// Structure for vm_region_basic_info
///
/// This matches the structure returned by `mach_vm_region()` with `VM_REGION_BASIC_INFO` flavor.
/// It contains information about a memory region's protection flags and attributes.
///
/// Note: This is named to match the C struct `vm_region_basic_info_data_t` from macOS headers,
/// but uses Rust naming conventions (PascalCase).
///
/// ## Field Descriptions
///
/// - `protection`: Current protection flags (read, write, execute)
/// - `max_protection`: Maximum allowed protection flags
/// - `inheritance`: How child processes inherit this region
/// - `shared`: Whether the region is shared between processes
/// - `reserved`: Reserved for future use
/// - `offset`: Offset into the mapped file (if applicable)
/// - `behavior`: Memory behavior hints (e.g., caching strategy)
/// - `user_wired_count`: Number of times the region is wired in user space
#[repr(C)]
pub struct VmRegionBasicInfoData
{
    /// Current protection flags (VM_PROT_READ, VM_PROT_WRITE, VM_PROT_EXECUTE)
    pub protection: u32,
    /// Maximum allowed protection flags
    pub max_protection: u32,
    /// Inheritance behavior for child processes
    pub inheritance: u32,
    /// Whether the region is shared between processes
    pub shared: u32,
    /// Reserved for future use
    pub reserved: u32,
    /// Offset into the mapped file (if applicable)
    pub offset: u64,
    /// Memory behavior hints (caching strategy, etc.)
    pub behavior: u32,
    /// Number of times the region is wired in user space
    pub user_wired_count: u16,
}

// Task and Process Management Functions
//
// These functions deal with Mach tasks (processes) and obtaining access to them.
#[link(name = "c", kind = "dylib")]
extern "C" {
    // Get a Mach port to a process by PID
    ///
    /// This function obtains a Mach task port for the process with the given PID.
    /// The task port allows you to control and inspect the process.
    ///
    /// ## Security
    ///
    /// This function requires special permissions:
    /// - Running as root (sudo)
    /// - Debugging entitlements (`com.apple.security.cs.debugger`)
    ///
    /// Without these permissions, the function will return `KERN_PROTECTION_FAILURE`.
    ///
    /// ## Parameters
    ///
    /// - `target_task`: Our own task port (use `mach_task_self()`)
    /// - `pid`: Process ID of the target process
    /// - `task`: Output parameter - receives the task port for the target process
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_PROTECTION_FAILURE` if permissions denied
    /// - `KERN_INVALID_ARGUMENT` if PID is invalid
    /// - `KERN_FAILURE` if process not found
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can access any process if you have permissions
    /// - The returned task port must be used carefully
    /// - Invalid PIDs can cause errors
    ///
    /// ## Documentation
    ///
    /// **Note**: `task_for_pid` is not publicly documented in Apple's current developer
    /// documentation due to security restrictions. It's a restricted Mach API that requires
    /// special entitlements. For implementation details, see:
    /// - XNU kernel source: [osfmk/kern/task.c](https://github.com/apple-oss-distributions/xnu)
    pub fn task_for_pid(target_task: mach_port_t, pid: c_int, task: *mut mach_port_t) -> kern_return_t;

    /// Deallocate a Mach port
    ///
    /// This function releases a Mach port that was previously obtained (e.g., from
    /// `task_for_pid()`). After deallocation, the port is no longer valid and cannot
    /// be used for further operations.
    ///
    /// ## Parameters
    ///
    /// - `target_task`: Task port that owns the port to deallocate (use `mach_task_self()`)
    /// - `name`: The Mach port to deallocate
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_RIGHT` if the port is invalid or already deallocated
    /// - `KERN_INVALID_TASK` if the target task is invalid
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can deallocate kernel resources
    /// - Deallocating an invalid port can cause errors
    /// - Double-deallocation can cause undefined behavior
    ///
    /// ## Documentation
    ///
    /// See: [mach_port_deallocate(3) man page](https://developer.apple.com/documentation/kernel/1578777-mach_port_deallocate/)
    pub fn mach_port_deallocate(target_task: mach_port_t, name: mach_port_t) -> kern_return_t;
}

// Thread State Functions
//
// These functions read and write thread state (registers) from threads.
#[link(name = "c", kind = "dylib")]
extern "C" {
    // Read thread state (registers) from a thread
    ///
    /// This function reads the CPU register values from a thread. The registers
    /// are returned as an array of `natural_t` values, with the format depending
    /// on the architecture flavor.
    ///
    /// ## Architecture Flavors
    ///
    /// - `ARM_THREAD_STATE64` (6): ARM64 registers
    /// - `X86_THREAD_STATE64` (4): x86-64 registers
    ///
    /// ## Parameters
    ///
    /// - `target_act`: Thread port (from `task_threads()`)
    /// - `flavor`: Architecture flavor (ARM_THREAD_STATE64, X86_THREAD_STATE64, etc.)
    /// - `old_state`: Output buffer for register values
    /// - `old_state_count`: Input/output - size of buffer / actual size used
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ARGUMENT` if flavor is invalid
    /// - `KERN_FAILURE` if thread is invalid
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It requires a valid thread port
    /// - The state buffer must be correctly sized for the flavor
    /// - Invalid flavors can cause undefined behavior
    ///
    /// See: [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
    pub fn thread_get_state(
        target_act: thread_act_t,
        flavor: c_int,
        old_state: *mut natural_t,
        old_state_count: *mut mach_msg_type_number_t,
    ) -> kern_return_t;

    /// Write thread state (registers) to a thread
    ///
    /// This function modifies the CPU register values in a thread. This can be
    /// used to change the program's execution flow (e.g., jump to a different address).
    ///
    /// ## ⚠️ Warning
    ///
    /// Modifying thread state can crash the process or cause undefined behavior.
    /// Only do this if you know what you're doing!
    ///
    /// ## Parameters
    ///
    /// - `target_act`: Thread port (from `task_threads()`)
    /// - `flavor`: Architecture flavor (ARM_THREAD_STATE64, X86_THREAD_STATE64, etc.)
    /// - `new_state`: Buffer containing new register values
    /// - `new_state_count`: Number of values in the buffer
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ARGUMENT` if flavor or state is invalid
    /// - `KERN_FAILURE` if thread is invalid
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can modify the thread's execution state
    /// - Invalid state can crash the process
    /// - Must match the architecture flavor exactly
    ///
    /// See: [thread_set_state(3) man page](https://developer.apple.com/documentation/kernel/1418827-thread_set_state/)
    pub fn thread_set_state(
        target_act: thread_act_t,
        flavor: c_int,
        new_state: *const natural_t,
        new_state_count: mach_msg_type_number_t,
    ) -> kern_return_t;

    /// Suspend a specific thread
    ///
    /// This function suspends execution of a single thread within a task. Unlike
    /// `task_suspend()`, which suspends all threads, this allows fine-grained control
    /// over individual threads.
    ///
    /// ## Parameters
    ///
    /// - `target_act`: Thread port (from `task_threads()`) to suspend
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ARGUMENT` if the thread port is invalid
    /// - `KERN_FAILURE` if the thread cannot be suspended
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can suspend thread execution
    /// - Invalid thread ports can cause errors
    /// - Suspending threads can cause deadlocks if not done carefully
    ///
    /// ## Architecture Notes
    ///
    /// On ARM64, `thread_suspend()` is the preferred method for per-thread control.
    /// On Intel, you may need to use `thread_set_state()` with `X86_THREAD_STATE64` flavor
    /// to coordinate per-thread operations.
    ///
    /// ## Documentation
    ///
    /// See: [thread_suspend(3) man page](https://developer.apple.com/documentation/kernel/1402804-thread_suspend/)
    pub fn thread_suspend(target_act: thread_act_t) -> kern_return_t;

    /// Resume a specific thread
    ///
    /// This function resumes execution of a single thread within a task. The thread
    /// will continue from where it was suspended.
    ///
    /// ## Parameters
    ///
    /// - `target_act`: Thread port (from `task_threads()`) to resume
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ARGUMENT` if the thread port is invalid
    /// - `KERN_FAILURE` if the thread cannot be resumed
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can resume thread execution
    /// - Invalid thread ports can cause errors
    /// - Resuming threads can cause race conditions if not coordinated properly
    ///
    /// ## Architecture Notes
    ///
    /// On ARM64, `thread_resume()` is the preferred method for per-thread control.
    /// On Intel, you may need to use `thread_set_state()` with `X86_THREAD_STATE64` flavor
    /// to coordinate per-thread operations.
    ///
    /// ## Documentation
    ///
    /// See: [thread_resume(3) man page](https://developer.apple.com/documentation/kernel/1402805-thread_resume/)
    pub fn thread_resume(target_act: thread_act_t) -> kern_return_t;
}

// Virtual Memory Functions
//
// These functions read and write memory in other processes, and enumerate
// memory regions.
#[link(name = "c", kind = "dylib")]
extern "C" {
    /// Read memory from a Mach task
    ///
    /// This function reads memory from the target process's address space.
    /// The memory is allocated in the current process's address space and
    /// must be deallocated using `vm_deallocate()`.
    ///
    /// ## Parameters
    ///
    /// - `target_task`: Task port (from `task_for_pid()`)
    /// - `address`: Virtual address in the target process
    /// - `size`: Number of bytes to read
    /// - `data`: Output parameter - receives pointer to allocated memory
    /// - `data_count`: Output parameter - receives actual bytes read
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ADDRESS` if address is invalid
    /// - `KERN_PROTECTION_FAILURE` if memory is not readable
    ///
    /// ## Memory Management
    ///
    /// **Important**: `vm_read()` allocates memory that you must free using `vm_deallocate()`.
    /// The memory is allocated in your process's address space, not the target's.
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can read from any address in the target process
    /// - Invalid addresses can cause errors
    /// - The returned memory must be deallocated
    ///
    /// See: [vm_read(3) man page](https://developer.apple.com/documentation/kernel/1585350-vm_read/)
    pub fn vm_read(
        target_task: vm_map_t,
        address: vm_address_t,
        size: vm_size_t,
        data: *mut vm_offset_t,
        data_count: *mut mach_msg_type_number_t,
    ) -> kern_return_t;

    /// Write memory to a Mach task
    ///
    /// This function writes memory to the target process's address space.
    /// The data is copied from the current process's address space.
    ///
    /// ## ⚠️ Warning
    ///
    /// Writing to memory can crash the target process or cause undefined behavior.
    /// Only write to writable memory regions (e.g., stack, heap).
    /// Writing to code segments may corrupt the program.
    ///
    /// ## Parameters
    ///
    /// - `target_task`: Task port (from `task_for_pid()`)
    /// - `address`: Virtual address in the target process
    /// - `data`: Pointer to data in current process's address space
    /// - `data_count`: Number of bytes to write
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ADDRESS` if address is invalid
    /// - `KERN_PROTECTION_FAILURE` if memory is not writable
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can modify memory in the target process
    /// - Invalid addresses can cause errors
    /// - Writing to read-only memory will fail
    ///
    /// See: [vm_write(3) man page](https://developer.apple.com/documentation/kernel/1585462-vm_write/)
    pub fn vm_write(
        target_task: vm_map_t,
        address: vm_address_t,
        data: vm_offset_t,
        data_count: mach_msg_type_number_t,
    ) -> kern_return_t;

    /// Deallocate memory allocated by vm_read()
    ///
    /// This function frees memory that was allocated by `vm_read()`. You must
    /// call this after copying data from the memory returned by `vm_read()`.
    ///
    /// ## Parameters
    ///
    /// - `target_task`: Task port (from `task_for_pid()`)
    /// - `address`: Address returned by `vm_read()`
    /// - `size`: Size returned by `vm_read()`
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ADDRESS` if address is invalid
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It can deallocate memory
    /// - Must only be called with addresses from `vm_read()`
    /// - Double-free can cause undefined behavior
    ///
    /// See: [vm_deallocate(3) man page](https://developer.apple.com/documentation/kernel/1585284-vm_deallocate/)
    pub fn vm_deallocate(target_task: vm_map_t, address: vm_address_t, size: vm_size_t) -> kern_return_t;

    /// Get information about a memory region (64-bit version)
    ///
    /// This function retrieves information about a memory region starting at
    /// the given address. On macOS 10.5+, `vm_region` was replaced by `mach_vm_region`
    /// for 64-bit applications.
    ///
    /// ## Parameters
    ///
    /// - `target_task`: Task port (from `task_for_pid()`)
    /// - `address`: Input/output - starting address (updated to actual region start)
    /// - `size`: Output parameter - receives size of the region
    /// - `flavor`: Information flavor (VM_REGION_BASIC_INFO = 9)
    /// - `info`: Output buffer for region information
    /// - `info_count`: Input/output - size of buffer / actual size used
    /// - `object_name`: Output parameter - receives object name (if applicable)
    ///
    /// ## Returns
    ///
    /// - `KERN_SUCCESS` (0) on success
    /// - `KERN_INVALID_ADDRESS` if address is beyond address space
    /// - `KERN_NO_SPACE` if no region found at address
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It requires a valid task port
    /// - The info buffer must be correctly sized
    /// - Invalid addresses can cause errors
    ///
    /// See: [mach_vm_region(3) man page](https://developer.apple.com/documentation/kernel/1402149-mach_vm_region/)
    pub fn mach_vm_region(
        target_task: vm_map_t,
        address: *mut u64, // mach_vm_address_t
        size: *mut u64,    // mach_vm_size_t
        flavor: c_int,
        info: *mut VmRegionBasicInfoData,
        info_count: *mut mach_msg_type_number_t,
        object_name: *mut mach_port_t,
    ) -> kern_return_t;
}

// Process Spawning Functions
//
// These functions spawn new processes under debugger control using posix_spawn.
#[link(name = "c", kind = "dylib")]
extern "C" {
    /// Spawn a new process with control over its initial state
    ///
    /// This function creates a new process from the given executable path and arguments.
    /// It provides fine-grained control over the process's initial state, including
    /// whether it starts suspended.
    ///
    /// ## Parameters
    ///
    /// - `pid`: Output parameter - receives the PID of the spawned process
    /// - `path`: Path to the executable to spawn
    /// - `file_actions`: File actions to perform (can be null)
    /// - `attrp`: Spawn attributes (controls behavior like POSIX_SPAWN_START_SUSPENDED)
    /// - `argv`: Array of argument strings (null-terminated)
    /// - `envp`: Array of environment variable strings (null-terminated, can be null)
    ///
    /// ## Returns
    ///
    /// - `0` on success
    /// - Non-zero errno value on failure
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It spawns a new process
    /// - Invalid paths or arguments can cause errors
    /// - The spawned process inherits the current process's permissions
    ///
    /// See: [posix_spawn(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/posix_spawn.2.html)
    pub fn posix_spawn(
        pid: *mut libc::pid_t,
        path: *const libc::c_char,
        file_actions: *const libc::posix_spawn_file_actions_t,
        attrp: *const libc::posix_spawnattr_t,
        argv: *const *const libc::c_char,
        envp: *const *const libc::c_char,
    ) -> libc::c_int;

    /// Initialize spawn attributes structure
    ///
    /// Initializes a `posix_spawnattr_t` structure for use with `posix_spawn()`.
    /// Must be called before setting any attributes.
    ///
    /// ## Parameters
    ///
    /// - `attrp`: Pointer to the attributes structure to initialize
    ///
    /// ## Returns
    ///
    /// - `0` on success
    /// - Non-zero errno value on failure
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It modifies the attributes structure
    /// - Must be paired with `posix_spawnattr_destroy()`
    ///
    /// See: [posix_spawnattr_init(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawnattr_init.3.html)
    pub fn posix_spawnattr_init(attrp: *mut libc::posix_spawnattr_t) -> libc::c_int;

    /// Destroy spawn attributes structure
    ///
    /// Frees resources associated with a `posix_spawnattr_t` structure.
    /// Must be called after use to avoid memory leaks.
    ///
    /// ## Parameters
    ///
    /// - `attrp`: Pointer to the attributes structure to destroy
    ///
    /// ## Returns
    ///
    /// - `0` on success
    /// - Non-zero errno value on failure
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It frees memory
    /// - Must only be called on initialized attributes
    ///
    /// See: [posix_spawnattr_destroy(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawnattr_destroy.3.html)
    pub fn posix_spawnattr_destroy(attrp: *mut libc::posix_spawnattr_t) -> libc::c_int;

    /// Set spawn flags
    ///
    /// Sets flags that control the behavior of the spawned process.
    /// Common flags include:
    /// - `POSIX_SPAWN_START_SUSPENDED`: Start the process in a suspended state
    ///
    /// ## Parameters
    ///
    /// - `attrp`: Pointer to the attributes structure
    /// - `flags`: Flags to set (bitwise OR of flag values)
    ///
    /// ## Returns
    ///
    /// - `0` on success
    /// - Non-zero errno value on failure
    ///
    /// ## Safety
    ///
    /// This function is unsafe because:
    /// - It modifies the attributes structure
    /// - Must be called on an initialized attributes structure
    ///
    /// See: [posix_spawnattr_setflags(3) man page](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawnattr_setflags.3.html)
    pub fn posix_spawnattr_setflags(attrp: *mut libc::posix_spawnattr_t, flags: libc::c_short) -> libc::c_int;
}

/// Spawn flags for posix_spawn
///
/// These constants define flags that can be passed to `posix_spawnattr_setflags()`.
/// They control the behavior of the spawned process.
pub mod spawn_flags
{
    use libc::c_short;

    /// Start the process in a suspended state
    ///
    /// When this flag is set, the spawned process will be created but will not
    /// start executing until explicitly resumed. This is essential for debuggers
    /// that need to set breakpoints before the process starts running.
    ///
    /// On macOS, this uses Mach's `task_suspend()` internally to suspend the
    /// process immediately after creation.
    ///
    /// See: [POSIX_SPAWN_START_SUSPENDED documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_spawn.3.html)
    pub const POSIX_SPAWN_START_SUSPENDED: c_short = 0x0080;
}
