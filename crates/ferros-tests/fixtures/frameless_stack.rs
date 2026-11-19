//! Frameless stack fixture program (arm64 macOS).
//!
//! Before `main`, dyld runs the initializer `frameless_caller`, which calls the leaf
//! `frameless_spill`: compact unwind `UNWIND_ARM64_MODE_FRAMELESS` with a 32-byte stack
//! allocation and an epilogue, written in assembly in `fixtures/asm/frameless_stack.s`
//! and linked by `build.rs`. Neither has a Rust symbol or a source line.
//!
//! Functions:
//! - `main`
//!
//! Markers:
//! - `frameless_done`

fn main()
{
    println!("frameless stack fixture done"); // BREAK: frameless_done
}
