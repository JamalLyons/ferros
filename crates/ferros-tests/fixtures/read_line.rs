//! Read-line fixture program.
//!
//! Reads one line from stdin and prints `read: <line>` without its newline, or
//! `read: <eof>` when stdin is at end of file (for example `/dev/null`).
//!
//! Functions:
//! - `main`
//!
//! Markers:
//! - `read_stdin`

use std::io::BufRead as _;

fn main()
{
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line); // BREAK: read_stdin
    match read {
        Ok(0) => println!("read: <eof>"),
        Ok(_) => println!("read: {}", line.trim_end_matches(['\r', '\n'])),
        Err(error) => println!("read: <error {error}>"),
    }
}
