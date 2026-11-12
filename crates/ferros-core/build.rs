//! Build script for ferros-core
//!
//! This script checks the minimum Rust version before compilation
//! (Edition 2024 = Rust 1.85.0+). ferros-core holds no OS-specific code, so it has
//! no operating-system checks; the macOS version checks live in ferros-macos.
//!
//! ## Requirements
//!
//! - **Rust**: Edition 2024 (Rust 1.85.0 or newer); the workspace pins the nightly
//!   toolchain via `rust-toolchain.toml`

fn main()
{
    // Check minimum Rust version
    // Edition 2024 requires Rust 1.85.0. The workspace uses nightly (see rust-toolchain.toml).
    if let Ok(rustc_version) = rustc_version::version() {
        let min_rust_version = rustc_version::Version::parse("1.85.0").unwrap();

        if rustc_version < min_rust_version {
            panic!(
                "ferros-core requires Rust {} or newer (Edition 2024), found {}",
                min_rust_version, rustc_version
            );
        }
    } else {
        // If we can't get version (e.g., in some build environments), just warn
        println!("cargo:warning=could not verify Rust version");
    }
}
