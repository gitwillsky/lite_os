mod dynamic_linking;
mod errno;
mod fs;
mod futex;
pub mod graphics;
mod memory;
mod process;
mod signal;
mod timer;
mod watchdog;

use crate::syscall::{fs::*, process::*};

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    match syscall_id {
        17 => sys_getcwd(args[0] as *mut u8, args[1]),

        23 => sys_dup(args[0]),
        24 => sys_dup3(args[0], args[1], args[2] as i32),
        25 => sys_fcntl(args[0], args[1] as i32, args[2]),

        63 => sys_read(args[0], args[1] as *const u8, args[2]),
        64 => sys_write(args[0], args[1] as *const u8, args[2]),

        93 => sys_exit(args[0] as i32),

        124 => sys_sched_yield(),

        172 => sys_get_pid(),
        173 => sys_get_ppid(),

        220 => sys_clone(
            args[0] as i32,      // flags
            args[1],             // child_stack
            args[2] as *mut i32, // parent_tid
            args[3] as *mut i32, // child_tid
            args[4],             // tls
        ),

        _ => {
            error!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
