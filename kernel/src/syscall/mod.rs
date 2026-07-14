mod credentials;
mod epoll;
mod errno;
mod eventfd;
mod fs;
mod futex;
mod ioctl;
mod membarrier;
mod memory;
mod poll;
mod process;
mod random;
mod reboot;
mod resource_limit;
mod riscv_hwprobe;
mod scheduler;
mod signal;
mod socket;
mod system_identity;
mod system_info;
mod timer;
mod tty;

use crate::syscall::{
    credentials::*, epoll::*, fs::*, futex::*, ioctl::*, memory::*, poll::*, process::*, random::*,
    reboot::*, scheduler::*, signal::*, socket::*, system_identity::*, system_info::*, timer::*,
};
use eventfd::sys_eventfd2;
use membarrier::sys_membarrier;
use resource_limit::sys_prlimit64;
use riscv_hwprobe::sys_riscv_hwprobe;
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
        SYSCALL_EPOLL_CREATE1 => sys_epoll_create1(args[0]),
        SYSCALL_EPOLL_CTL => sys_epoll_ctl(args[0], args[1], args[2], args[3]),
        SYSCALL_EPOLL_PWAIT => sys_epoll_pwait(
            args[0],
            args[1],
            args[2],
            args[3] as isize,
            args[4],
            args[5],
        ),
        SYSCALL_GETCWD => sys_get_cwd(args[0] as *mut u8, args[1]),
        SYSCALL_EVENTFD2 => sys_eventfd2(args[0] as u32, args[1] as u32),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2] as u32),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1] as u32, args[2]),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_FLOCK => sys_flock(args[0], args[1]),
        SYSCALL_MKNODAT => sys_mknodat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u64,
        ),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2]),
        SYSCALL_SYMLINKAT => {
            sys_symlinkat(args[0] as *const u8, args[1] as isize, args[2] as *const u8)
        }
        SYSCALL_LINKAT => sys_linkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_FACCESSAT => sys_faccessat(args[0] as isize, args[1] as *const u8, args[2]),
        SYSCALL_FCHMOD => sys_fchmod(args[0], args[1] as u32),
        SYSCALL_FCHMODAT => sys_fchmodat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_FCHOWNAT => sys_fchownat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
            args[4] as u32,
        ),
        SYSCALL_FCHOWN => sys_fchown(args[0], args[1] as u32, args[2] as u32),
        SYSCALL_STATFS => fs::statistics::sys_statfs(args[0] as *const u8, args[1]),
        SYSCALL_FSTATFS => fs::statistics::sys_fstatfs(args[0], args[1]),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1] as u64),
        SYSCALL_FALLOCATE => sys_fallocate(args[0], args[1], args[2] as i64, args[3] as i64),
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
        SYSCALL_PREAD64 => sys_pread64(args[0], args[1], args[2], args[3] as i64),
        SYSCALL_PWRITE64 => sys_pwrite64(args[0], args[1], args[2], args[3] as i64),
        SYSCALL_PREADV => sys_preadv(args[0], args[1], args[2], args[3] as i64),
        SYSCALL_PWRITEV => sys_pwritev(args[0], args[1], args[2], args[3] as i64),
        SYSCALL_PPOLL => sys_ppoll(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_PSELECT6 => sys_pselect6(args[0], args[1], args[2], args[3], args[4], args[5]),
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
        SYSCALL_FDATASYNC => sys_fdatasync(args[0]),
        SYSCALL_PREADV2 => sys_preadv2(args[0], args[1], args[2], args[3] as i64, args[5] as u32),
        SYSCALL_PWRITEV2 => sys_pwritev2(args[0], args[1], args[2], args[3] as i64, args[5] as u32),
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
        SYSCALL_FUTEX => sys_futex(
            args[0],
            args[1],
            args[2] as u32,
            args[3],
            args[4],
            args[5] as u32,
        ),
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
        SYSCALL_NANOSLEEP => sys_nanosleep(
            args[0] as *const timer::TimeSpec,
            args[1] as *mut timer::TimeSpec,
        ),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as i32, args[1] as *mut timer::TimeSpec),
        SYSCALL_CLOCK_GETRES => sys_clock_getres(args[0] as i32, args[1] as *mut timer::TimeSpec),
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(
            args[0] as i32,
            args[1] as i32,
            args[2] as *const timer::TimeSpec,
            args[3] as *mut timer::TimeSpec,
        ),
        SYSCALL_SCHED_SETPARAM => sys_sched_setparam(args[0] as i32, args[1]),
        SYSCALL_SCHED_SETSCHEDULER => {
            sys_sched_setscheduler(args[0] as i32, args[1] as i32, args[2])
        }
        SYSCALL_SCHED_GETSCHEDULER => sys_sched_getscheduler(args[0] as i32),
        SYSCALL_SCHED_GETPARAM => sys_sched_getparam(args[0] as i32, args[1]),
        SYSCALL_SCHED_SETAFFINITY => sys_sched_setaffinity(args[0] as i32, args[1] as u32, args[2]),
        SYSCALL_SCHED_GETAFFINITY => sys_sched_getaffinity(args[0] as i32, args[1] as u32, args[2]),
        SYSCALL_SCHED_YIELD => sys_sched_yield(),
        SYSCALL_SCHED_GET_PRIORITY_MAX => sys_sched_get_priority_max(args[0] as i32),
        SYSCALL_SCHED_GET_PRIORITY_MIN => sys_sched_get_priority_min(args[0] as i32),
        SYSCALL_SCHED_RR_GET_INTERVAL => sys_sched_rr_get_interval(args[0] as i32, args[1]),
        SYSCALL_KILL => sys_kill(args[0] as i32, args[1]),
        SYSCALL_TKILL => sys_tkill(args[0], args[1]),
        SYSCALL_TGKILL => sys_tgkill(args[0], args[1], args[2]),
        SYSCALL_SIGALTSTACK => sys_sigaltstack(args[0], args[1]),
        SYSCALL_RT_SIGSUSPEND => sys_rt_sigsuspend(args[0], args[1]),
        SYSCALL_RT_SIGACTION => sys_rt_sigaction(args[0], args[1], args[2], args[3]),
        SYSCALL_RT_SIGPROCMASK => sys_rt_sigprocmask(args[0], args[1], args[2], args[3]),
        SYSCALL_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(args[0], args[1], args[2], args[3]),
        SYSCALL_RT_SIGRETURN => sys_rt_sigreturn(),
        SYSCALL_SETPRIORITY => sys_setpriority(args[0] as i32, args[1] as u32, args[2] as i32),
        SYSCALL_GETPRIORITY => sys_getpriority(args[0] as i32, args[1] as u32),
        SYSCALL_REBOOT => sys_reboot(args[0], args[1], args[2], args[3]),
        SYSCALL_SETGID => sys_set_id(false, args[0] as u32),
        SYSCALL_SETUID => sys_set_id(true, args[0] as u32),
        SYSCALL_SETRESUID => {
            sys_set_res_ids(true, [args[0] as u32, args[1] as u32, args[2] as u32])
        }
        SYSCALL_GETRESUID => sys_get_res_ids(true, [args[0], args[1], args[2]]),
        SYSCALL_SETRESGID => {
            sys_set_res_ids(false, [args[0] as u32, args[1] as u32, args[2] as u32])
        }
        SYSCALL_GETRESGID => sys_get_res_ids(false, [args[0], args[1], args[2]]),
        SYSCALL_SETPGID => sys_setpgid(args[0], args[1]),
        SYSCALL_GETPGID => sys_getpgid(args[0]),
        SYSCALL_GETSID => sys_getsid(args[0]),
        SYSCALL_SETSID => sys_setsid(),
        SYSCALL_GETGROUPS => sys_getgroups(args[0], args[1]),
        SYSCALL_SETGROUPS => sys_setgroups(args[0], args[1]),
        SYSCALL_UNAME => sys_uname(args[0]),
        SYSCALL_GETTIMEOFDAY => sys_gettimeofday(args[0], args[1]),
        SYSCALL_GETITIMER => sys_getitimer(args[0], args[1]),
        SYSCALL_SETITIMER => sys_setitimer(args[0], args[1], args[2]),
        SYSCALL_UMASK => sys_umask(args[0] as u32),
        SYSCALL_GETCPU => sys_getcpu(args[0], args[1], args[2]),
        SYSCALL_GETPID => sys_get_pid(),
        SYSCALL_GETPPID => sys_get_ppid(),
        SYSCALL_GETUID => sys_get_id(true, false),
        SYSCALL_GETEUID => sys_get_id(true, true),
        SYSCALL_GETGID => sys_get_id(false, false),
        SYSCALL_GETEGID => sys_get_id(false, true),
        SYSCALL_GETTID => sys_get_tid(),
        SYSCALL_SOCKET => sys_socket(args[0], args[1], args[2]),
        SYSCALL_SOCKETPAIR => sys_socketpair(args[0], args[1], args[2], args[3]),
        SYSCALL_BIND => sys_bind(args[0], args[1], args[2]),
        SYSCALL_LISTEN => sys_listen(args[0], args[1] as isize),
        SYSCALL_ACCEPT => sys_accept(args[0], args[1], args[2]),
        SYSCALL_CONNECT => sys_connect(args[0], args[1], args[2]),
        SYSCALL_GETSOCKNAME => sys_getsockname(args[0], args[1], args[2]),
        SYSCALL_GETPEERNAME => sys_getpeername(args[0], args[1], args[2]),
        SYSCALL_SENDTO => sys_sendto(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_RECVFROM => sys_recvfrom(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_SENDMSG => sys_sendmsg(args[0], args[1], args[2]),
        SYSCALL_RECVMSG => sys_recvmsg(args[0], args[1], args[2]),
        SYSCALL_SETSOCKOPT => sys_setsockopt(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_GETSOCKOPT => sys_getsockopt(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_SHUTDOWN => sys_shutdown(args[0], args[1]),
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
        SYSCALL_MSYNC => sys_msync(args[0], args[1], args[2]),
        SYSCALL_MADVISE => sys_madvise(args[0], args[1], args[2]),
        SYSCALL_GETRANDOM => sys_getrandom(args[0], args[1], args[2]),
        SYSCALL_MEMBARRIER => sys_membarrier(args[0], args[1], args[2]),
        SYSCALL_WAIT4 => sys_wait4(
            args[0] as isize,
            args[1] as *mut i32,
            args[2],
            args[3] as *mut u8,
        ),
        SYSCALL_PRLIMIT64 => sys_prlimit64(args[0], args[1], args[2], args[3]),
        SYSCALL_ACCEPT4 => sys_accept4(args[0], args[1], args[2], args[3]),
        SYSCALL_RISCV_HWPROBE => sys_riscv_hwprobe(args[0], args[1], args[2], args[3], args[4]),
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
