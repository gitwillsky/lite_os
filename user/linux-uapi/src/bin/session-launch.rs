//! Single-threaded session setup and exec trampoline.

use std::ffi::{OsStr, OsString};

use linux_uapi::process::{Pid, exec_session_child};

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let parent = arguments
        .next()
        .and_then(|value| value.to_string_lossy().parse::<i32>().ok())
        .and_then(Pid::new)
        .unwrap_or_else(|| usage());
    if arguments.next().as_deref() != Some(OsStr::new("--")) {
        usage();
    }
    let program = arguments.next().unwrap_or_else(|| usage());
    let target_arguments: Vec<OsString> = arguments.collect();
    exec_session_child(parent, &program, &target_arguments);
}

fn usage() -> ! {
    eprintln!("usage: session-launch <parent-pid> -- <program> [argument ...]");
    std::process::exit(127);
}
