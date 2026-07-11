mod errno;
mod fs;
mod memory;
mod process;
mod timer;

use crate::syscall::{fs::*, memory::*, process::*, timer::*};
use syscall_abi::*;

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    match syscall_id {
        SYSCALL_GETCWD => sys_get_cwd(args[0] as *mut u8, args[1]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_NANOSLEEP => sys_nanosleep(
            args[0] as *const timer::TimeSpec,
            args[1] as *mut timer::TimeSpec,
        ),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as i32, args[1] as *mut timer::TimeSpec),
        SYSCALL_SCHED_YIELD => sys_sched_yield(),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_GETPID => sys_get_pid(),
        SYSCALL_GETTID => sys_get_tid(),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_EXECVE => sys_execve(
            args[0] as *const u8,        // path
            args[1] as *const *const u8, // argv
            args[2] as *const *const u8, // envp
        ),
        _ => {
            error!("syscall: invalid syscall_id: {}", syscall_id);
            -errno::ENOSYS
        }
    }
}
