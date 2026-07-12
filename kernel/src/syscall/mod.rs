mod errno;
mod fs;
mod futex;
mod memory;
mod poll;
mod process;
mod random;
mod reboot;
mod signal;
mod system_identity;
mod system_info;
mod timer;
mod tty;

use crate::syscall::{
    fs::*, futex::*, memory::*, poll::*, process::*, random::*, reboot::*, signal::*,
    system_identity::*, system_info::*, timer::*, tty::*,
};
use syscall_abi::*;

const INTERNAL_RESTART_SYS: isize = isize::MIN;
pub(crate) const INTERRUPTED_RESULT: isize = -errno::EINTR;

/// @description syscall dispatcher 向 trap layer 返回的唯一控制结果。
pub(crate) enum SyscallOutcome {
    /// 将 Linux 返回值或负 errno 写回 `a0`。
    Return(isize),
    /// 暂存为 `EINTR`，并由实际交付 signal 的 disposition 决定是否重放 ecall。
    Restart,
}

/// @description 解码一个 Linux/riscv64 syscall，并隔离不得暴露给用户态的内部重启结果。
///
/// @param syscall_id `a7` 中的 Linux/riscv64 syscall number。
/// @param args `a0..a5` 中的六个原始参数。
/// @return 普通返回值/负 errno，或只允许 trap layer 消费的重启控制结果。
pub(crate) fn syscall(syscall_id: usize, args: [usize; 6]) -> SyscallOutcome {
    let result = match syscall_id {
        SYSCALL_GETCWD => sys_get_cwd(args[0] as *mut u8, args[1]),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2] as u32),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1] as u32, args[2]),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2]),
        SYSCALL_STATFS => fs::statistics::sys_statfs(args[0] as *const u8, args[1]),
        SYSCALL_FSTATFS => fs::statistics::sys_fstatfs(args[0], args[1]),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1] as u64),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_OPENAT => sys_openat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE2 => sys_pipe2(args[0], args[1] as u32),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as i64, args[2] as u32),
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_READV => sys_readv(args[0], args[1], args[2]),
        SYSCALL_WRITEV => sys_writev(args[0], args[1], args[2]),
        SYSCALL_PPOLL => sys_ppoll(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_READLINKAT => sys_readlinkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_NEWFSTATAT => sys_newfstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3] as u32,
        ),
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut u8),
        SYSCALL_SYNC => sys_sync(),
        SYSCALL_FSYNC => sys_fsync(args[0]),
        SYSCALL_UTIMENSAT => sys_utimensat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const timer::TimeSpec,
            args[3] as u32,
        ),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0]),
        SYSCALL_FUTEX => sys_futex(args[0], args[1], args[2] as u32, args[3]),
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
        SYSCALL_NANOSLEEP => sys_nanosleep(
            args[0] as *const timer::TimeSpec,
            args[1] as *mut timer::TimeSpec,
        ),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as i32, args[1] as *mut timer::TimeSpec),
        SYSCALL_SCHED_YIELD => sys_sched_yield(),
        SYSCALL_KILL => sys_kill(args[0] as i32, args[1]),
        SYSCALL_TKILL => sys_tkill(args[0], args[1]),
        SYSCALL_TGKILL => sys_tgkill(args[0], args[1], args[2]),
        SYSCALL_RT_SIGSUSPEND => sys_rt_sigsuspend(args[0], args[1]),
        SYSCALL_RT_SIGACTION => sys_rt_sigaction(args[0], args[1], args[2], args[3]),
        SYSCALL_RT_SIGPROCMASK => sys_rt_sigprocmask(args[0], args[1], args[2], args[3]),
        SYSCALL_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(args[0], args[1], args[2], args[3]),
        SYSCALL_RT_SIGRETURN => sys_rt_sigreturn(),
        SYSCALL_REBOOT => sys_reboot(args[0], args[1], args[2], args[3]),
        SYSCALL_SETPGID => sys_setpgid(args[0], args[1]),
        SYSCALL_GETPGID => sys_getpgid(args[0]),
        SYSCALL_GETSID => sys_getsid(args[0]),
        SYSCALL_SETSID => sys_setsid(),
        SYSCALL_UNAME => sys_uname(args[0]),
        SYSCALL_GETTIMEOFDAY => sys_gettimeofday(args[0], args[1]),
        SYSCALL_GETPID => sys_get_pid(),
        SYSCALL_GETPPID => sys_get_ppid(),
        SYSCALL_GETUID | SYSCALL_GETEUID | SYSCALL_GETGID | SYSCALL_GETEGID => {
            sys_get_root_identity()
        }
        SYSCALL_GETTID => sys_get_tid(),
        SYSCALL_SYSINFO => sys_sysinfo(args[0]),
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
        SYSCALL_GETRANDOM => sys_getrandom(args[0], args[1], args[2]),
        SYSCALL_WAIT4 => sys_wait4(
            args[0] as isize,
            args[1] as *mut i32,
            args[2],
            args[3] as *mut u8,
        ),
        _ => {
            debug!("syscall: unsupported syscall_id: {}", syscall_id);
            -errno::ENOSYS
        }
    };
    if result == INTERNAL_RESTART_SYS {
        SyscallOutcome::Restart
    } else {
        SyscallOutcome::Return(result)
    }
}
