//! Tests for error handling

use ferros_core::error::{DebuggerError, Result};
#[cfg(target_os = "macos")]
use ferros_core::platform::macos::error::MachError;

#[cfg(target_os = "macos")]
#[test]
fn test_mach_error_protection_failure()
{
    let error = MachError::ProtectionFailure;
    let message = format!("{}", error);
    assert!(message.contains("Permission denied"));
}

#[cfg(target_os = "macos")]
#[test]
fn test_mach_error_invalid_argument()
{
    let error = MachError::InvalidArgument;
    let message = format!("{}", error);
    assert!(message.contains("Invalid") || message.contains("invalid"));
}

#[cfg(target_os = "macos")]
#[test]
fn test_mach_error_process_not_found()
{
    let error = MachError::ProcessNotFound;
    let message = format!("{}", error);
    assert!(message.contains("not found") || message.contains("Process"));
}

#[cfg(target_os = "macos")]
#[test]
fn test_mach_error_unknown()
{
    let error = MachError::Unknown(999);
    let message = format!("{}", error);
    assert!(message.contains("999"));
}

#[cfg(target_os = "macos")]
#[test]
fn test_mach_error_to_debugger_error()
{
    let mach_err = MachError::ProtectionFailure;
    let debugger_err: DebuggerError = mach_err.into();

    match debugger_err {
        DebuggerError::MachError(_) => {
            // Expected: MachError should convert to MachError variant
        }
        _ => panic!("Expected MachError variant"),
    }
}

#[test]
fn test_debugger_error_display()
{
    let error = DebuggerError::ProcessNotFound(12345);
    let message = format!("{}", error);
    assert!(message.contains("12345"));
    assert!(message.contains("not found"));
}

#[test]
fn test_debugger_error_permission_denied()
{
    let error = DebuggerError::PermissionDenied("test reason".to_string());
    let message = format!("{}", error);
    assert!(message.contains("Permission denied"));
    assert!(message.contains("test reason"));
}

#[test]
fn test_debugger_error_invalid_argument()
{
    let error = DebuggerError::InvalidArgument("test arg".to_string());
    let message = format!("{}", error);
    assert!(message.contains("Invalid argument"));
    assert!(message.contains("test arg"));
}

#[test]
fn test_result_type()
{
    // Test that Result type is properly aliased
    let _result: Result<()> = Ok(());
    let _error_result: Result<()> = Err(DebuggerError::ProcessNotFound(12345));
}
