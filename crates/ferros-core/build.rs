//! Build script for ferros-core
//!
//! This script validates system requirements before compilation:
//! - Minimum Rust version (Edition 2021 = Rust 1.56.0+)
//! - Platform-specific requirements (macOS version, etc.)
//! - Architecture support
//!
//! ## Requirements
//!
//! - **Rust**: Edition 2021 (Rust 1.56.0 or newer)
//! - **macOS**: 10.9+ (Mavericks) for Intel, 11.0+ (Big Sur) for Apple Silicon
//! - **Linux**: TBD
//! - **Windows**: TBD

/// Minimum required Rust version (Edition 2021)
const MIN_RUST_VERSION: &str = "1.56.0";

/// Minimum macOS version for Intel (x86_64)
/// mach_vm_region() was introduced in macOS 10.5, but we require 10.9+
/// as that's when 64-bit support became standard
#[cfg(target_os = "macos")]
#[allow(dead_code)] // Only used when target_arch = "x86_64"
const MIN_MACOS_VERSION_INTEL: (u32, u32, u32) = (10, 9, 0);

/// Minimum macOS version for Apple Silicon (ARM64)
/// Apple Silicon requires macOS 11.0+ (Big Sur)
#[cfg(target_os = "macos")]
const MIN_MACOS_VERSION_ARM64: (u32, u32, u32) = (11, 0, 0);

fn main()
{
    check_rust_version();
    check_platform_requirements();
}

/// Verifies that the Rust compiler meets the minimum version requirement.
fn check_rust_version()
{
    match rustc_version::version() {
        Ok(version) => {
            let min_version = rustc_version::Version::parse(MIN_RUST_VERSION).expect("invalid MIN_RUST_VERSION constant");

            if version < min_version {
                panic!(
                    "ferros-core requires Rust {} or newer (Edition 2021), found {}",
                    min_version, version
                );
            }
        }
        Err(_) => {
            // If we can't get version (e.g., in some build environments), just warn
            // This allows builds to proceed in cross-compilation scenarios
            println!("cargo:warning=could not verify Rust version");
        }
    }
}

/// Orchestrates platform-specific requirement checks.
fn check_platform_requirements()
{
    #[cfg(target_os = "macos")]
    check_macos_requirements();
}

#[cfg(target_os = "macos")]
/// Validates macOS version requirements based on architecture.
fn check_macos_requirements()
{
    let Some(version) = get_macos_version() else {
        // If we can't detect macOS version, warn but don't fail
        // (might be cross-compiling or in a non-standard build environment)
        println!("cargo:warning=could not detect macOS version");
        return;
    };

    // Check architecture-specific requirements
    #[cfg(target_arch = "aarch64")]
    {
        if version < MIN_MACOS_VERSION_ARM64 {
            panic!(
                "ferros-core on Apple Silicon requires macOS {}.{}.{} or newer (Big Sur+), found {}.{}.{}",
                MIN_MACOS_VERSION_ARM64.0,
                MIN_MACOS_VERSION_ARM64.1,
                MIN_MACOS_VERSION_ARM64.2,
                version.0,
                version.1,
                version.2
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    {
        if version < MIN_MACOS_VERSION_INTEL {
            panic!(
                "ferros-core requires macOS {}.{}.{} or newer, found {}.{}.{}",
                MIN_MACOS_VERSION_INTEL.0,
                MIN_MACOS_VERSION_INTEL.1,
                MIN_MACOS_VERSION_INTEL.2,
                version.0,
                version.1,
                version.2
            );
        }
    }
}

#[cfg(target_os = "macos")]
/// Attempts to detect the current macOS version.
///
/// Returns `None` if version detection fails (e.g., cross-compiling or
/// non-standard build environment).
fn get_macos_version() -> Option<(u32, u32, u32)>
{
    use std::process::Command;

    let output = Command::new("sw_vers").arg("-productVersion").output().ok()?;

    let version_str = String::from_utf8(output.stdout).ok()?;
    let version_str = version_str.trim();

    // Parse version string (e.g., "14.2.1" or "11.0.0")
    let parts: Vec<&str> = version_str.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let major = parts[0].parse::<u32>().ok()?;
    let minor = parts[1].parse::<u32>().ok()?;
    let patch = parts.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);

    Some((major, minor, patch))
}
