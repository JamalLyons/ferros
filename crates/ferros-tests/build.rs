//! Builds the assembly and C code linked into individual arm64 macOS fixtures, plus
//! the dynamic library loaded by the `dyld_aliases` fixture.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `(source, compiler flags, fixture)`.
static SOURCES: [(&str, &[&str], &str); 6] = [
    ("fixtures/asm/frameless_stack.s", &[], "frameless_stack"),
    ("fixtures/c/long_jump.c", &["-g", "-O0"], "long_jump"),
    ("fixtures/c/pthread_once.c", &["-g", "-O0"], "pthread_once"),
    ("fixtures/c/soft_faults.c", &["-g", "-O0"], "soft_faults"),
    ("fixtures/c/dyld_aliases.c", &["-g", "-O0"], "dyld_aliases"),
    ("fixtures/c/values_registers.c", &["-g", "-O2"], "values_basic"),
];

const ALIAS_LIBRARY_SOURCE: &str = "fixtures/c/alias_library.c";

fn main()
{
    for (source, _, _) in &SOURCES {
        println!("cargo::rerun-if-changed={source}");
    }
    println!("cargo::rerun-if-changed={ALIAS_LIBRARY_SOURCE}");
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
    let library = out_dir.join("libferros_alias_fixture.dylib");
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-dynamiclib"])
        .arg(ALIAS_LIBRARY_SOURCE)
        .arg("-o")
        .arg(&library)
        .status()
        .expect("run cc to build the dyld alias library");
    assert!(status.success(), "cc failed to build {ALIAS_LIBRARY_SOURCE}: {status}");
}
