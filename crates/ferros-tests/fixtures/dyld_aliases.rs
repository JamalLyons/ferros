//! The C program in `fixtures/c/dyld_aliases.c` opens the same library through
//! several paths before the debugger inspects the image list.
#![cfg_attr(all(target_os = "macos", target_arch = "aarch64"), no_main)]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main()
{
    eprintln!("the dyld_aliases fixture is built only for arm64 macOS");
    std::process::exit(1);
}
