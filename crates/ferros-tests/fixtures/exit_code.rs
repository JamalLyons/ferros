//! Exit code fixture program.
//!
//! Functions:
//! - `main`
//!
//! Markers:
//! - `before_exit`

fn main()
{
    std::process::exit(7); // BREAK: before_exit
}
