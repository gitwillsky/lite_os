mod errno;
mod fs;
mod memory;
mod process;
mod timer;

use crate::syscall::{fs::*, memory::*, process::*, timer::*};
use syscall_abi::*;

pub(crate) fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    match syscall_id {
        SYSCALL_GETCWD => sys_get_cwd(args[0] as *mut u8, args[1]),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2] as u32),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1] as u32, args[2]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2]),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1] as u64),
        SYSCALL_OPENAT => sys_openat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as i64, args[2] as u32),
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_NEWFSTATAT => sys_newfstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3] as u32,
        ),
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut u8),
        SYSCALL_FSYNC => sys_fsync(args[0]),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_EXIT | SYSCALL_EXIT_GROUP => sys_exit(args[0] as i32),
        SYSCALL_NANOSLEEP => sys_nanosleep(
            args[0] as *const timer::TimeSpec,
            args[1] as *mut timer::TimeSpec,
        ),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as i32, args[1] as *mut timer::TimeSpec),
        SYSCALL_SCHED_YIELD => sys_sched_yield(),
        SYSCALL_GETPID => sys_get_pid(),
        SYSCALL_GETPPID => sys_get_ppid(),
        SYSCALL_GETTID => sys_get_tid(),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_EXECVE => sys_execve(
            args[0] as *const u8,        // path
            args[1] as *const *const u8, // argv
            args[2] as *const *const u8, // envp
        ),
        SYSCALL_MMAP => sys_mmap(
            args[0],
            args[1],
            args[2],
            args[3],
            args[4] as isize,
            args[5],
        ),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        SYSCALL_WAIT4 => sys_wait4(
            args[0] as isize,
            args[1] as *mut i32,
            args[2],
            args[3] as *mut u8,
        ),
        _ => {
            error!("syscall: invalid syscall_id: {}", syscall_id);
            -errno::ENOSYS
        }
    }
}
