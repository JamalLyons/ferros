//! Builds the non-Rust code of two fixtures and links each into its fixture only: the
//! arm64 assembly of `frameless_stack` (see `fixtures/asm/frameless_stack.s`) and the C
//! program of `long_jump` (see `fixtures/c/long_jump.c`).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `(source, compiler flags, fixture)`.
static SOURCES: [(&str, &[&str], &str); 2] = [
    ("fixtures/asm/frameless_stack.s", &[], "frameless_stack"),
    ("fixtures/c/long_jump.c", &["-g", "-O0"], "long_jump"),
];

fn main()
{
    for (source, _, _) in &SOURCES {
        println!("cargo::rerun-if-changed={source}");
    }
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os != "macos" || arch != "aarch64" {
        return;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    for &(source, flags, fixture) in &SOURCES {
        let stem = Path::new(source).file_stem().expect("source has a file name");
        let object = out_dir.join(stem).with_extension("o");
        let status = Command::new("cc")
            .args(["-arch", "arm64", "-c"])
            .args(flags)
            .arg(source)
            .arg("-o")
            .arg(&object)
            .status()
            .unwrap_or_else(|err| panic!("run cc to build {source}: {err}"));
        assert!(status.success(), "cc failed to build {source}: {status}");
        println!("cargo::rustc-link-arg-bin=ferros-fixture-{fixture}={}", object.display());
    }
}
