//! Assembles the arm64 code of the `frameless_stack` fixture and links it into that
//! fixture only (see `fixtures/asm/frameless_stack.s`).

use std::env;
use std::path::PathBuf;
use std::process::Command;

const SOURCE: &str = "fixtures/asm/frameless_stack.s";

fn main()
{
    println!("cargo::rerun-if-changed={SOURCE}");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os != "macos" || arch != "aarch64" {
        return;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let object = out_dir.join("frameless_stack.o");
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-c"])
        .arg(SOURCE)
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap_or_else(|err| panic!("run cc to assemble {SOURCE}: {err}"));
    assert!(status.success(), "cc failed to assemble {SOURCE}: {status}");
    println!(
        "cargo::rustc-link-arg-bin=ferros-fixture-frameless_stack={}",
        object.display()
    );
}
