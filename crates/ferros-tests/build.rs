//! Builds the assembly and C code linked into individual arm64 macOS fixtures, plus
//! the dynamic library loaded by the `dyld_aliases` fixture, and the standalone packed-DWARF fixture.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `(source, compiler flags, fixture)`.
static SOURCES: [(&str, &[&str], &str); 7] = [
    ("fixtures/asm/frameless_stack.s", &[], "frameless_stack"),
    ("fixtures/asm/values_misc.s", &[], "values_misc"),
    ("fixtures/c/long_jump.c", &["-g", "-O0"], "long_jump"),
    ("fixtures/c/pthread_once.c", &["-g", "-O0"], "pthread_once"),
    ("fixtures/c/soft_faults.c", &["-g", "-O0"], "soft_faults"),
    ("fixtures/c/dyld_aliases.c", &["-g", "-O0"], "dyld_aliases"),
    ("fixtures/c/values_registers.c", &["-g", "-O2"], "values_basic"),
];

const ALIAS_LIBRARY_SOURCE: &str = "fixtures/c/alias_library.c";
const PACKED_SOURCE: &str = "fixtures/values_packed.rs";
const PACKED_FLAGS: &[&str] = &[
    "--crate-name",
    "probe",
    "--edition",
    "2024",
    "-g",
    "-Ccodegen-units=4",
    "-Csplit-debuginfo=packed",
];

fn main()
{
    for (source, _, _) in &SOURCES {
        println!("cargo::rerun-if-changed={source}");
    }
    println!("cargo::rerun-if-changed={ALIAS_LIBRARY_SOURCE}");
    println!("cargo::rerun-if-changed={PACKED_SOURCE}");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" {
        return;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    build_packed_fixture(&out_dir);
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("aarch64") {
        return;
    }
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
    // The assembly fixture has an interior label. Omit the linker's function-start
    // list, which can classify that label as an entry.
    println!("cargo::rustc-link-arg-bin=ferros-fixture-values_misc=-Wl,-no_function_starts");
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

fn build_packed_fixture(out_dir: &Path)
{
    // Use the standalone rustc layout from #415. Cargo's incremental build
    // partitions the source differently and did not reproduce the CU overlap.
    let packed = out_dir.join("ferros-fixture-values_packed");
    let status = Command::new(env::var_os("RUSTC").expect("RUSTC is set by cargo"))
        .arg("--target")
        .arg(env::var_os("TARGET").expect("TARGET is set by cargo"))
        .args(PACKED_FLAGS)
        .arg(PACKED_SOURCE)
        .arg("-o")
        .arg(&packed)
        .status()
        .expect("run rustc to build the packed-DWARF fixture");
    assert!(status.success(), "rustc failed to build {PACKED_SOURCE}: {status}");
    println!(
        "cargo::rustc-env=CARGO_BIN_EXE_ferros-fixture-values_packed={}",
        packed.display()
    );
}
