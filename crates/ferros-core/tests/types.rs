//! Tests for platform-agnostic types

use ferros_core::types::{MemoryRegion, ProcessId, Registers};

#[test]
fn test_process_id_from_u32()
{
    let pid = ProcessId::from(12345);
    assert_eq!(pid.0, 12345);
}

#[test]
fn test_process_id_to_u32()
{
    let pid = ProcessId::from(54321);
    let value: u32 = pid.into();
    assert_eq!(value, 54321);
}

#[test]
fn test_process_id_equality()
{
    let pid1 = ProcessId::from(12345);
    let pid2 = ProcessId::from(12345);
    let pid3 = ProcessId::from(54321);

    assert_eq!(pid1, pid2);
    assert_ne!(pid1, pid3);
}

#[test]
fn test_registers_new()
{
    let regs = Registers::new();
    assert_eq!(regs.pc, 0);
    assert_eq!(regs.sp, 0);
    assert_eq!(regs.fp, 0);
    assert_eq!(regs.general.len(), 0);
    assert_eq!(regs.status, 0);
}

#[test]
fn test_registers_default()
{
    let regs = Registers::default();
    assert_eq!(regs.pc, 0);
    assert_eq!(regs.sp, 0);
    assert_eq!(regs.fp, 0);
    assert_eq!(regs.general.len(), 0);
    assert_eq!(regs.status, 0);
}

#[test]
fn test_memory_region_new()
{
    let region = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), Some("[heap]".to_string()));

    assert_eq!(region.start, 0x1000);
    assert_eq!(region.end, 0x2000);
    assert_eq!(region.permissions, "rwx");
    assert_eq!(region.name, Some("[heap]".to_string()));
}

#[test]
fn test_memory_region_new_no_name()
{
    let region = MemoryRegion::new(0x1000, 0x2000, "r-x".to_string(), None);

    assert_eq!(region.start, 0x1000);
    assert_eq!(region.end, 0x2000);
    assert_eq!(region.permissions, "r-x");
    assert_eq!(region.name, None);
}

#[test]
fn test_memory_region_size()
{
    let region = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), None);
    assert_eq!(region.size(), 0x1000);

    let large_region = MemoryRegion::new(0x0, 0x1000000, "rwx".to_string(), None);
    assert_eq!(large_region.size(), 0x1000000);
}

#[test]
fn test_memory_region_size_zero()
{
    // Edge case: end <= start should return 0 (using saturating_sub)
    let region = MemoryRegion::new(0x2000, 0x1000, "rwx".to_string(), None);
    assert_eq!(region.size(), 0);

    let same_region = MemoryRegion::new(0x1000, 0x1000, "rwx".to_string(), None);
    assert_eq!(same_region.size(), 0);
}

#[test]
fn test_memory_region_is_readable()
{
    let readable = MemoryRegion::new(0x1000, 0x2000, "r-x".to_string(), None);
    assert!(readable.is_readable());

    let writable_only = MemoryRegion::new(0x1000, 0x2000, "w-x".to_string(), None);
    assert!(!writable_only.is_readable());

    let rwx = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), None);
    assert!(rwx.is_readable());
}

#[test]
fn test_memory_region_is_writable()
{
    let writable = MemoryRegion::new(0x1000, 0x2000, "rw-".to_string(), None);
    assert!(writable.is_writable());

    let readable_only = MemoryRegion::new(0x1000, 0x2000, "r--".to_string(), None);
    assert!(!readable_only.is_writable());

    let rwx = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), None);
    assert!(rwx.is_writable());
}

#[test]
fn test_memory_region_is_executable()
{
    let executable = MemoryRegion::new(0x1000, 0x2000, "r-x".to_string(), None);
    assert!(executable.is_executable());

    let readable_only = MemoryRegion::new(0x1000, 0x2000, "r--".to_string(), None);
    assert!(!readable_only.is_executable());

    let rwx = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), None);
    assert!(rwx.is_executable());
}

#[test]
fn test_memory_region_permissions_combinations()
{
    // Test various permission combinations
    let code = MemoryRegion::new(0x1000, 0x2000, "r-x".to_string(), None);
    assert!(code.is_readable());
    assert!(!code.is_writable());
    assert!(code.is_executable());

    let data = MemoryRegion::new(0x2000, 0x3000, "rw-".to_string(), None);
    assert!(data.is_readable());
    assert!(data.is_writable());
    assert!(!data.is_executable());

    let ro_data = MemoryRegion::new(0x3000, 0x4000, "r--".to_string(), None);
    assert!(ro_data.is_readable());
    assert!(!ro_data.is_writable());
    assert!(!ro_data.is_executable());
}

#[test]
fn test_memory_region_equality()
{
    let region1 = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), Some("[heap]".to_string()));
    let region2 = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), Some("[heap]".to_string()));
    let region3 = MemoryRegion::new(0x2000, 0x3000, "rwx".to_string(), Some("[heap]".to_string()));

    assert_eq!(region1, region2);
    assert_ne!(region1, region3);
}

#[test]
fn test_memory_region_clone()
{
    let region = MemoryRegion::new(0x1000, 0x2000, "rwx".to_string(), Some("[heap]".to_string()));
    let cloned = region.clone();

    assert_eq!(region, cloned);
    // Verify they're independent
    assert_eq!(cloned.start, 0x1000);
    assert_eq!(cloned.end, 0x2000);
}
