//! Build script for ferros-core
//!
//! This script checks system requirements before compilation:
//! - Minimum Rust version (Edition 2021 = Rust 1.56.0+)
//! - Platform-specific requirements (macOS version, etc.)
//! - Architecture support
//!
//! ## Requirements
//!
//! - **Rust**: Edition 2021 (Rust 1.56.0 or newer)
//! - **macOS**: 10.9+ (Mavericks) for Intel, 11.0+ (Big Sur) for Apple Silicon
//! - **Linux**: TBD (ptrace support)
//! - **Windows**: TBD (WinDbg API support)

fn main()
{
    // Check minimum Rust version
    // Edition 2021 requires Rust 1.56.0
    // Note: We use nightly Rust, but the minimum stable version for edition 2021 is 1.56.0
    if let Ok(rustc_version) = rustc_version::version() {
        let min_rust_version = rustc_version::Version::parse("1.56.0").unwrap();

        if rustc_version < min_rust_version {
            panic!(
                "ferros-core requires Rust {} or newer (Edition 2021), found {}",
                min_rust_version, rustc_version
            );
        }
    } else {
        // If we can't get version (e.g., in some build environments), just warn
        println!("cargo:warning=could not verify Rust version");
    }

    // Platform-specific checks
    #[cfg(target_os = "macos")]
    check_macos_requirements();

    #[cfg(target_arch = "aarch64")]
    #[cfg(target_os = "macos")]
    check_macos_arm64_requirements();
}

#[cfg(target_os = "macos")]
fn check_macos_requirements()
{
    // Check macOS version
    // mach_vm_region() was introduced in macOS 10.5 (Leopard)
    // However, for practical purposes, we require macOS 10.9+ (Mavericks)
    // as that's when 64-bit support became standard
    let min_macos_version = (10, 9, 0);

    if let Some(version) = get_macos_version() {
        if version < min_macos_version {
            panic!(
                "ferros-core requires macOS {}.{}.{} or newer, found {}.{}.{}",
                min_macos_version.0, min_macos_version.1, min_macos_version.2, version.0, version.1, version.2
            );
        }
    } else {
        // If we can't detect macOS version, warn but don't fail
        // (might be cross-compiling)
        println!("cargo:warning=could not detect macOS version");
    }
}

#[cfg(target_arch = "aarch64")]
#[cfg(target_os = "macos")]
fn check_macos_arm64_requirements()
{
    // Apple Silicon (ARM64) requires macOS 11.0+ (Big Sur)
    let min_macos_version = (11, 0, 0);

    if let Some(version) = get_macos_version() {
        if version < min_macos_version {
            panic!(
                "ferros-core on Apple Silicon requires macOS {}.{}.{} or newer (Big Sur+), found {}.{}.{}",
                min_macos_version.0, min_macos_version.1, min_macos_version.2, version.0, version.1, version.2
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn get_macos_version() -> Option<(u32, u32, u32)>
{
    // Try to get macOS version from environment or system
    // This is a best-effort check - may not work in all build environments
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
