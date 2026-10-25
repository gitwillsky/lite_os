//! Single-threaded session setup and exec trampoline.

use std::ffi::{OsStr, OsString};

use linux_uapi::process::{Identity, Pid, exec_session_child};

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let parent = arguments
        .next()
        .and_then(|value| value.to_string_lossy().parse::<i32>().ok())
        .and_then(Pid::new)
        .unwrap_or_else(|| usage());
    let mut separator = arguments.next();
    let user = if separator.as_deref() == Some(OsStr::new("--user")) {
        let uid = arguments
            .next()
            .and_then(|s| s.to_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| usage());
        let gid = arguments
            .next()
            .and_then(|s| s.to_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| usage());
        let groups = arguments
            .next()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| usage());
        let groups = if groups.is_empty() {
            Vec::new()
        } else {
            groups
                .split(',')
                .map(|s| s.parse::<u32>().unwrap_or_else(|_| usage()))
                .collect()
        };
        separator = arguments.next();
        Some((Identity { uid, gid }, groups))
    } else {
        None
    };
    let pty = separator.as_deref() == Some(OsStr::new("--pty"));
    if pty {
        separator = arguments.next();
    }
    if separator.as_deref() != Some(OsStr::new("--")) {
        usage();
    }
    let program = arguments.next().unwrap_or_else(|| usage());
    let target_arguments: Vec<OsString> = arguments.collect();
    exec_session_child(parent, &program, &target_arguments, user, pty);
}

fn usage() -> ! {
    eprintln!(
        "usage: session-launch <parent-pid> [--user uid gid groups] [--pty] -- <program> [argument ...]"
    );
    std::process::exit(127);
}
