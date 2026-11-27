//! FFI and OS strings for the formatter integration test.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::hint::black_box;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

fn main()
{
    let c_string = CString::new("cstr").unwrap();
    let c_str: &CStr = c"borrowed";
    let os_string = OsString::from("os");
    let os_str = OsStr::new("os\tstr");
    let path_buf = PathBuf::from("/tmp/x");
    let path = Path::new("/tmp/café");
    let invalid = OsString::from_vec(vec![b'a', 0xff, b'b', 0xc3]);
    let boxed_path: Box<Path> = Path::new("/boxed").into();
    let boxed_c_str: Box<CStr> = c"boxed c".into();
    let boxed_os_str: Box<OsStr> = OsStr::new("boxed os").into();
    let mut owned_path: Box<Path> = Path::new("/mut").into();
    let mut_path: &mut Path = &mut owned_path;
    // An invalid byte inside the preview, then a character the 256-byte limit cuts short.
    let mut long_bytes = vec![0xff];
    long_bytes.resize(255, b'a');
    long_bytes.extend("é".bytes().chain([b'z'; 8]));
    let long_os = OsString::from_vec(long_bytes);
    let text = "plain";
    let owned = String::from("owned\n");
    let boxed_str: Box<str> = "boxed".into();

    black_box(&c_string); // BREAK: inspect
    black_box((&c_str, &os_string, &os_str, &path_buf, &path, &invalid));
    black_box((&boxed_path, &long_os, &text, &owned, &boxed_str));
    black_box((&boxed_c_str, &boxed_os_str, &mut_path));
}
