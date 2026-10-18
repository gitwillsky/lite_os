mod capabilities;
mod clone_errno;
mod credentials;
mod device;
mod epoll;
use syscall_abi::errno;
mod eventfd;
mod fs;
mod futex;
mod getrandom_flags;
mod ioctl;
mod ipc;
mod membarrier;
mod memfd;
mod memory;
mod mmap_flags;
mod pidfd;
mod poll;
mod process;
mod process_context;
mod process_control;
mod random;
mod reboot;
mod resource_limit;
mod resource_usage;
mod riscv_hwprobe;
mod scheduler;
mod seccomp;
mod signal;
mod signalfd;
mod socket;
mod system_identity;
mod system_info;
mod timer;
mod timerfd;
mod unimplemented_report;
mod user_iovec;

mod restart;

use crate::syscall::{
    credentials::*, epoll::*, fs::*, futex::*, ioctl::*, memory::*, poll::*, process::*, random::*,
    reboot::*, scheduler::*, signal::*, socket::*, system_identity::*, system_info::*, timer::*,
};
use eventfd::sys_eventfd2;
use membarrier::sys_membarrier;
use memfd::sys_memfd_create;
use process_control::sys_prctl;
use resource_limit::sys_prlimit64;
use resource_usage::sys_getrusage;
use riscv_hwprobe::sys_riscv_hwprobe;
use syscall_abi::*;
use timerfd::{sys_timerfd_create, sys_timerfd_gettime, sys_timerfd_settime};

const INTERNAL_RESTART_SYS: isize = isize::MIN;
// 只用于 dispatcher；不向用户泄漏，不能与 ERESTARTSYS 的 handler 规则合并。
const INTERNAL_RESTART_NO_HANDLER: isize = isize::MIN + 1;
// 实际 Thread restart block 已保存冻结输入；无 handler 时只重放标准 restart_syscall。
const INTERNAL_RESTART_BLOCK: isize = isize::MIN + 2;
pub(crate) const INTERRUPTED_RESULT: isize = -errno::EINTR;

pub(in crate::syscall) fn file_descriptor_error(error: crate::file::FileDescriptorError) -> isize {
    -match error {
        crate::file::FileDescriptorError::NotFound => errno::EBADF,
        crate::file::FileDescriptorError::Limit => errno::EMFILE,
        crate::file::FileDescriptorError::OutOfMemory => errno::ENOMEM,
        crate::file::FileDescriptorError::Busy => errno::EBUSY,
    }
}

/// syscall dispatcher 向 trap layer 返回的唯一控制结果。
pub(crate) enum SyscallOutcome {
    /// 将 Linux 返回值或负 errno 写回 architecture syscall return register。
    Return(isize),
    /// exec 已替换 UserContext，或 seccomp TRAP 保留原 registers；不写 completion 或 arm restart。
    PreserveRegisters,
    /// 暂存为 `EINTR`，并由实际交付 signal 的 disposition 决定是否重放 syscall instruction。
    Restart(crate::task::SyscallRestartPolicy),
}

/// 解码一个 asm-generic Linux syscall，并隔离不得暴露给用户态的内部重启结果。
///
/// # Parameters
///
/// - `syscall_id`: architecture syscall-number register 中的 asm-generic Linux syscall number。
/// - `args`: 六个 syscall argument register 的原始值。
/// - `current`: trap 已持有的 calling Thread；交给 seccomp 消费，任何 handler 前释放。
///
/// # Returns
///
/// 普通返回值/负 errno，或只允许 trap layer 消费的重启控制结果。
pub(crate) fn syscall(
    syscall_id: usize,
    args: [usize; 6],
    current: alloc::sync::Arc<crate::task::TaskControlBlock>,
) -> SyscallOutcome {
    if let Some(outcome) = seccomp::enforce(current, syscall_id, args) {
        return outcome;
    }
    let result = match crate::system::decode_architecture_syscall(syscall_id) {
        None => match syscall_id {
            SYSCALL_SETXATTR => {
                fs::xattr::set(-100, args[0], 0, args[1], args[2], args[3], args[4] as u32)
            }
            SYSCALL_LSETXATTR => fs::xattr::set(
                -100,
                args[0],
                fs::xattr::NOFOLLOW,
                args[1],
                args[2],
                args[3],
                args[4] as u32,
            ),
            SYSCALL_FSETXATTR => fs::xattr::set(
                args[0] as i32 as isize,
                0,
                fs::xattr::EMPTY_PATH,
                args[1],
                args[2],
                args[3],
                args[4] as u32,
            ),
            SYSCALL_GETXATTR => fs::xattr::get(-100, args[0], 0, args[1], args[2], args[3]),
            SYSCALL_LGETXATTR => fs::xattr::get(
                -100,
                args[0],
                fs::xattr::NOFOLLOW,
                args[1],
                args[2],
                args[3],
            ),
            SYSCALL_FGETXATTR => fs::xattr::get(
                args[0] as i32 as isize,
                0,
                fs::xattr::EMPTY_PATH,
                args[1],
                args[2],
                args[3],
            ),
            SYSCALL_LISTXATTR => fs::xattr::list(-100, args[0], 0, args[1], args[2]),
            SYSCALL_LLISTXATTR => {
                fs::xattr::list(-100, args[0], fs::xattr::NOFOLLOW, args[1], args[2])
            }
            SYSCALL_FLISTXATTR => fs::xattr::list(
                args[0] as i32 as isize,
                0,
                fs::xattr::EMPTY_PATH,
                args[1],
                args[2],
            ),
            SYSCALL_REMOVEXATTR => fs::xattr::remove(-100, args[0], 0, args[1]),
            SYSCALL_LREMOVEXATTR => fs::xattr::remove(-100, args[0], fs::xattr::NOFOLLOW, args[1]),
            SYSCALL_FREMOVEXATTR => {
                fs::xattr::remove(args[0] as i32 as isize, 0, fs::xattr::EMPTY_PATH, args[1])
            }
            SYSCALL_SETXATTRAT => fs::xattr::set_at(args),
            SYSCALL_GETXATTRAT => fs::xattr::get_at(args),
            SYSCALL_LISTXATTRAT => fs::xattr::list(
                args[0] as i32 as isize,
                args[1],
                args[2] as u32,
                args[3],
                args[4],
            ),
            SYSCALL_REMOVEXATTRAT => {
                fs::xattr::remove(args[0] as i32 as isize, args[1], args[2] as u32, args[3])
            }
            SYSCALL_SWAPON => sys_swapon(args[0], args[1] as u32),
            SYSCALL_SWAPOFF => sys_swapoff(args[0]),
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
            SYSCALL_INOTIFY_INIT1 => sys_inotify_init1(args[0] as u32),
            SYSCALL_INOTIFY_ADD_WATCH => {
                sys_inotify_add_watch(args[0], args[1] as *const u8, args[2] as u32)
            }
            SYSCALL_INOTIFY_RM_WATCH => sys_inotify_rm_watch(args[0], args[1] as i32),
            SYSCALL_TIMERFD_CREATE => sys_timerfd_create(args[0] as i32, args[1] as u32),
            SYSCALL_SIGNALFD4 => {
                signalfd::sys_signalfd4(args[0] as i32, args[1], args[2], args[3] as u32)
            }
            SYSCALL_TIMERFD_SETTIME => {
                sys_timerfd_settime(args[0], args[1] as u32, args[2], args[3])
            }
            SYSCALL_TIMERFD_GETTIME => sys_timerfd_gettime(args[0], args[1]),
            SYSCALL_DUP => sys_dup(args[0]),
            SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2] as u32),
            SYSCALL_FCNTL => sys_fcntl(args[0], args[1] as u32, args[2]),
            SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
            SYSCALL_IOPRIO_SET => sys_ioprio_set(args[0] as i32, args[1] as i32, args[2] as i32),
            SYSCALL_IOPRIO_GET => sys_ioprio_get(args[0] as i32, args[1] as i32),
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
            SYSCALL_FCHMODAT => {
                sys_fchmodat(args[0] as isize, args[1] as *const u8, args[2] as u32)
            }
            SYSCALL_FCHOWNAT => sys_fchownat(
                args[0] as isize,
                args[1] as *const u8,
                args[2] as u32,
                args[3] as u32,
                args[4] as u32,
            ),
            SYSCALL_FCHOWN => sys_fchown(args[0], args[1] as u32, args[2] as u32),
            SYSCALL_MOUNT => fs::mount::sys_mount(
                args[0] as *const u8,
                args[1] as *const u8,
                args[2] as *const u8,
                args[3],
                args[4] as *const u8,
            ),
            SYSCALL_UMOUNT2 => fs::mount::sys_umount2(args[0] as *const u8, args[1]),
            SYSCALL_STATFS => fs::statistics::sys_statfs(args[0] as *const u8, args[1]),
            SYSCALL_FSTATFS => fs::statistics::sys_fstatfs(args[0], args[1]),
            SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1] as u64),
            SYSCALL_FALLOCATE => sys_fallocate(args[0], args[1], args[2] as i64, args[3] as i64),
            SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
            SYSCALL_FCHDIR => sys_fchdir(args[0]),
            SYSCALL_CHROOT => sys_chroot(args[0] as *const u8),
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
            SYSCALL_SENDFILE => sys_sendfile(args[0], args[1], args[2], args[3]),
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
            SYSCALL_PREADV2 => {
                sys_preadv2(args[0], args[1], args[2], args[3] as i64, args[5] as u32)
            }
            SYSCALL_PWRITEV2 => {
                sys_pwritev2(args[0], args[1], args[2], args[3] as i64, args[5] as u32)
            }
            SYSCALL_UTIMENSAT => sys_utimensat(
                args[0] as isize,
                args[1] as *const u8,
                args[2] as *const timer::TimeSpec,
                args[3] as u32,
            ),
            SYSCALL_RENAMEAT => sys_renameat2(
                args[0] as isize,
                args[1] as *const u8,
                args[2] as isize,
                args[3] as *const u8,
                0,
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
            SYSCALL_SEMGET => ipc::sys_semget(args[0] as i32, args[1] as i32, args[2] as u32),
            SYSCALL_SHMGET => ipc::sys_shmget(args[0] as i32, args[1], args[2] as u32),
            SYSCALL_SHMCTL => ipc::sys_shmctl(args[0] as i32, args[1] as i32, args[2]),
            SYSCALL_SHMAT => ipc::sys_shmat(args[0] as i32, args[1], args[2] as u32),
            SYSCALL_SHMDT => ipc::sys_shmdt(args[0]),
            SYSCALL_SEMCTL => {
                ipc::sys_semctl(args[0] as i32, args[1] as i32, args[2] as i32, args[3])
            }
            SYSCALL_SEMOP => ipc::sys_semop(args[0] as i32, args[1], args[2] as u32 as usize),
            SYSCALL_SEMTIMEDOP => {
                ipc::sys_semtimedop(args[0] as i32, args[1], args[2] as u32 as usize, args[3])
            }
            SYSCALL_MSGGET => ipc::sys_msgget(args[0] as i32, args[1] as u32),
            SYSCALL_MSGCTL => ipc::sys_msgctl(args[0] as i32, args[1] as i32, args[2]),
            SYSCALL_MSGRCV => ipc::sys_msgrcv(
                args[0] as i32,
                args[1],
                args[2],
                args[3] as i64,
                args[4] as u32,
            ),
            SYSCALL_MSGSND => ipc::sys_msgsnd(args[0] as i32, args[1], args[2], args[3] as u32),
            SYSCALL_UNSHARE => process_context::sys_unshare(args[0]),
            SYSCALL_SETNS => process_context::sys_setns(args[0], args[1] as u32),
            SYSCALL_RESTART_SYSCALL => restart::sys_restart_syscall(),
            SYSCALL_FUTEX => sys_futex(
                args[0],
                args[1],
                args[2] as u32,
                args[3],
                args[4],
                args[5] as u32,
            ),
            SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
            SYSCALL_GET_ROBUST_LIST => sys_get_robust_list(args[0] as i32, args[1], args[2]),
            SYSCALL_NANOSLEEP => sys_nanosleep(
                args[0] as *const timer::TimeSpec,
                args[1] as *mut timer::TimeSpec,
            ),
            SYSCALL_TIMER_CREATE => sys_timer_create(args[0] as i32, args[1], args[2]),
            SYSCALL_TIMER_GETTIME => sys_timer_gettime(args[0] as i32, args[1]),
            SYSCALL_TIMER_GETOVERRUN => sys_timer_getoverrun(args[0] as i32),
            SYSCALL_TIMER_SETTIME => {
                sys_timer_settime(args[0] as i32, args[1] as i32, args[2], args[3])
            }
            SYSCALL_TIMER_DELETE => sys_timer_delete(args[0] as i32),
            SYSCALL_CLOCK_GETTIME => {
                sys_clock_gettime(args[0] as i32, args[1] as *mut timer::TimeSpec)
            }
            SYSCALL_CLOCK_GETRES => {
                sys_clock_getres(args[0] as i32, args[1] as *mut timer::TimeSpec)
            }
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
            SYSCALL_SCHED_SETAFFINITY => {
                sys_sched_setaffinity(args[0] as i32, args[1] as u32, args[2])
            }
            SYSCALL_SCHED_GETAFFINITY => {
                sys_sched_getaffinity(args[0] as i32, args[1] as u32, args[2])
            }
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
            SYSCALL_RT_SIGPENDING => sys_rt_sigpending(args[0], args[1]),
            SYSCALL_RT_SIGQUEUEINFO => sys_rt_sigqueueinfo(args[0] as i32, args[1], args[2]),
            SYSCALL_RT_TGSIGQUEUEINFO => {
                sys_rt_tgsigqueueinfo(args[0] as i32, args[1] as i32, args[2], args[3])
            }
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
            SYSCALL_SETFSUID => sys_set_fs_id(true, args[0] as u32),
            SYSCALL_SETFSGID => sys_set_fs_id(false, args[0] as u32),
            SYSCALL_SETPGID => sys_setpgid(args[0] as i32, args[1] as i32),
            SYSCALL_GETPGID => sys_getpgid(args[0] as i32),
            SYSCALL_GETSID => sys_getsid(args[0] as i32),
            SYSCALL_SETSID => sys_setsid(),
            SYSCALL_GETGROUPS => sys_getgroups(args[0], args[1]),
            SYSCALL_SETGROUPS => sys_setgroups(args[0], args[1]),
            SYSCALL_CAPGET => capabilities::sys_capget(args[0], args[1]),
            SYSCALL_CAPSET => capabilities::sys_capset(args[0], args[1]),
            SYSCALL_UNAME => sys_uname(args[0]),
            SYSCALL_SETHOSTNAME => sys_set_uts_name(true, args[0], args[1]),
            SYSCALL_SETDOMAINNAME => sys_set_uts_name(false, args[0], args[1]),
            SYSCALL_GETTIMEOFDAY => sys_gettimeofday(args[0], args[1]),
            SYSCALL_GETITIMER => sys_getitimer(args[0], args[1]),
            SYSCALL_SETITIMER => sys_setitimer(args[0], args[1], args[2]),
            SYSCALL_UMASK => sys_umask(args[0] as u32),
            SYSCALL_PRCTL => sys_prctl(args[0], args[1], [args[2], args[3], args[4]]),
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
            SYSCALL_CLONE3 => sys_clone3(args[0], args[1]),
            SYSCALL_PIDFD_OPEN => pidfd::sys_pidfd_open(args[0] as i32, args[1] as u32),
            SYSCALL_PIDFD_GETFD => {
                pidfd::sys_pidfd_getfd(args[0] as i32, args[1] as i32, args[2] as u32)
            }
            SYSCALL_PIDFD_SEND_SIGNAL => pidfd::sys_pidfd_send_signal(
                args[0] as i32,
                args[1] as i32,
                args[2],
                args[3] as u32,
            ),
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
            SYSCALL_SECCOMP => seccomp::sys_seccomp(args[0] as u32, args[1] as u32, args[2]),
            SYSCALL_MEMFD_CREATE => sys_memfd_create(args[0], args[1] as u32),
            SYSCALL_MEMBARRIER => sys_membarrier(args[0], args[1], args[2]),
            SYSCALL_WAIT4 => sys_wait4(
                args[0] as i32 as isize,
                args[1] as *mut i32,
                args[2] as u32,
                args[3] as *mut u8,
            ),
            SYSCALL_WAITID => process::sys_waitid(
                args[0] as i32,
                args[1] as i32,
                args[2],
                args[3] as u32,
                args[4],
            ),
            SYSCALL_PRLIMIT64 => {
                sys_prlimit64(args[0] as i32, args[1] as u32 as usize, args[2], args[3])
            }
            SYSCALL_GETRUSAGE => sys_getrusage(args[0] as i32, args[1]),
            SYSCALL_ACCEPT4 => sys_accept4(args[0], args[1], args[2], args[3]),
            _ => {
                unimplemented_report::report(syscall_id);
                -errno::ENOSYS
            }
        },
        Some(syscall) => match syscall {
            SYSCALL_RISCV_HWPROBE => sys_riscv_hwprobe(args[0], args[1], args[2], args[3], args[4]),
            _ => unreachable!("architecture decoder returned an unowned syscall"),
        },
    };
    if result == INTERNAL_RESTART_SYS {
        SyscallOutcome::Restart(crate::task::SyscallRestartPolicy::WithRestartHandler)
    } else if result == INTERNAL_RESTART_NO_HANDLER {
        SyscallOutcome::Restart(crate::task::SyscallRestartPolicy::WithoutHandler)
    } else if result == INTERNAL_RESTART_BLOCK {
        SyscallOutcome::Restart(crate::task::SyscallRestartPolicy::RestartBlock)
    } else if syscall_id == SYSCALL_EXECVE && result == 0 {
        // 只有实际 handler 成功才代表替换；seccomp ERRNO|0 已在 dispatch 前返回 Return(0)。
        SyscallOutcome::PreserveRegisters
    } else {
        SyscallOutcome::Return(result)
    }
}
