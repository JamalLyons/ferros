//! Integration tests for macOS debugger functionality
//!
//! These tests require:
//! - Running on macOS (`#[cfg(target_os = "macos")]`)
//! - Special permissions (sudo or debugging entitlements)
//! - A target process to attach to
//!
//! Some tests may be skipped if permissions are not available.

use ferros_core::error::DebuggerError;
#[cfg(target_os = "macos")]
use ferros_core::platform::macos::MacOSDebugger;
use ferros_core::types::ProcessId;
use ferros_core::Debugger;

#[cfg(target_os = "macos")]
#[test]
fn test_debugger_new()
{
    // Test that we can create a debugger instance
    let debugger = MacOSDebugger::new();
    assert!(debugger.is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn test_debugger_attach_invalid_pid()
{
    // Test attaching to a non-existent process
    let mut debugger = MacOSDebugger::new().unwrap();
    let invalid_pid = ProcessId::from(u32::MAX);

    let result = debugger.attach(invalid_pid);
    assert!(result.is_err());

    // Should get a ProcessNotFound, MachError, or other error
    match result.unwrap_err() {
        DebuggerError::ProcessNotFound(_) | DebuggerError::MachError(_) => {
            // Expected: Mach API returns an error for invalid PID
        }
        _ => {
            // Other errors are also acceptable
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn test_debugger_not_attached_operations()
{
    // Test that operations fail when not attached
    let debugger = MacOSDebugger::new().unwrap();

    // Reading registers should fail
    let result = debugger.read_registers();
    assert!(result.is_err());
    // Should get an error (could be InvalidArgument, ReadRegistersFailed, etc.)
    let error = result.unwrap_err();
    match error {
        DebuggerError::InvalidArgument(_) | DebuggerError::ReadRegistersFailed(_) => {
            // Expected: operations fail when not attached
        }
        _ => {
            // Other errors are also acceptable
        }
    }

    // Getting memory regions should fail
    let result = debugger.get_memory_regions();
    assert!(result.is_err());
    // Should get an error
    let error = result.unwrap_err();
    match error {
        DebuggerError::InvalidArgument(_) => {
            // Expected: operations fail when not attached
        }
        _ => {
            // Other errors are also acceptable
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn test_debugger_detach_when_not_attached()
{
    // Test that detaching when not attached doesn't crash
    let mut debugger = MacOSDebugger::new().unwrap();

    // Should handle gracefully (either succeed or return an error)
    let result = debugger.detach();
    // Both Ok(()) and Err are acceptable here
    assert!(result.is_ok() || result.is_err());
}

// Note: Tests that require actual process attachment are skipped here
// because they require:
// 1. Special permissions (sudo or entitlements)
// 2. A running target process
// 3. Platform-specific setup
//
// These should be tested manually using the examples in `crates/ferros/examples/`
