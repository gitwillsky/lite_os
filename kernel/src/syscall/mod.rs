mod fs;
mod process;
mod signal;
mod timer;
mod dynamic_linking;
mod memory;
mod errno;

use fs::*;
use process::*;
use signal::*;
use dynamic_linking::*;
use memory::*;

pub use signal::sys_sigreturn;

const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_FORK: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_EXECVE: usize = 222;
const SYSCALL_WAIT: usize = 260;
const SYSCALL_SHUTDOWN: usize = 110;

// 文件系统系统调用
const SYSCALL_OPEN: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_LISTDIR: usize = 500;
const SYSCALL_MKDIR: usize = 501;
const SYSCALL_REMOVE: usize = 502;
const SYSCALL_STAT: usize = 80;
const SYSCALL_READ_FILE: usize = 503;
const SYSCALL_CHDIR: usize = 504;
const SYSCALL_GETCWD: usize = 505;
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_PIPE: usize = 59;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP2: usize = 24;
const SYSCALL_FLOCK: usize = 143;
const SYSCALL_MKFIFO: usize = 506;
const SYSCALL_CHMOD: usize = 507;
const SYSCALL_CHOWN: usize = 508;

// 调度相关系统调用
const SYSCALL_SETPRIORITY: usize = 141;
const SYSCALL_GETPRIORITY: usize = 140;
const SYSCALL_SCHED_SETSCHEDULER: usize = 144;
const SYSCALL_SCHED_GETSCHEDULER: usize = 145;

// 信号相关系统调用
const SYSCALL_KILL: usize = 129;
const SYSCALL_SIGNAL: usize = 48;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_SIGRETURN: usize = 139;
const SYSCALL_PAUSE: usize = 34;
const SYSCALL_ALARM: usize = 37;

// 权限相关系统调用
const SYSCALL_GETUID: usize = 102;
const SYSCALL_GETGID: usize = 104;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETGID: usize = 147;
const SYSCALL_GETEUID: usize = 107;
const SYSCALL_GETEGID: usize = 108;
const SYSCALL_SETEUID: usize = 148;
const SYSCALL_SETEGID: usize = 149;

// 动态链接相关系统调用
const SYSCALL_DLOPEN: usize = 600;
const SYSCALL_DLSYM: usize = 601;
const SYSCALL_DLCLOSE: usize = 602;

// 内存管理系统调用
const SYSCALL_BRK: usize = 214;
const SYSCALL_SBRK: usize = 215;
const SYSCALL_MMAP: usize = 223;
const SYSCALL_MUNMAP: usize = 216;

pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    match syscall_id {
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => sys_exec(args[0] as *const u8),
        SYSCALL_EXECVE => sys_execve(args[0] as *const u8, args[1] as *const *const u8, args[2] as *const *const u8),
        SYSCALL_WAIT => sys_wait_pid(args[0] as isize, args[1] as *mut i32),
        SYSCALL_SHUTDOWN => sys_shutdown(),

        // 文件系统系统调用
        SYSCALL_OPEN => sys_open(args[0] as *const u8, args[1] as u32),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_LISTDIR => sys_listdir(args[0] as *const u8, args[1] as *mut u8, args[2]),
        SYSCALL_MKDIR => sys_mkdir(args[0] as *const u8),
        SYSCALL_REMOVE => sys_remove(args[0] as *const u8),
        SYSCALL_STAT => sys_stat(args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_READ_FILE => sys_read_file(args[0] as *const u8, args[1] as *mut u8, args[2]),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as isize, args[2]),
        SYSCALL_PIPE => sys_pipe(args[0] as *mut i32),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP2 => sys_dup2(args[0], args[1]),
        SYSCALL_FLOCK => sys_flock(args[0], args[1] as i32),
        SYSCALL_MKFIFO => sys_mkfifo(args[0] as *const u8, args[1] as u32),
        SYSCALL_CHMOD => sys_chmod(args[0] as *const u8, args[1] as u32),
        SYSCALL_CHOWN => sys_chown(args[0] as *const u8, args[1] as u32, args[2] as u32),

        // 调度相关系统调用
        SYSCALL_SETPRIORITY => sys_setpriority(args[0] as i32, args[1] as i32, args[2] as i32),
        SYSCALL_GETPRIORITY => sys_getpriority(args[0] as i32, args[1] as i32),
        SYSCALL_SCHED_SETSCHEDULER => sys_sched_setscheduler(args[0] as i32, args[1] as i32, args[2] as *const u8),
        SYSCALL_SCHED_GETSCHEDULER => sys_sched_getscheduler(args[0] as i32),

        // 信号相关系统调用
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_SIGNAL => sys_signal(args[1] as u32, args[2]),
        SYSCALL_SIGACTION => sys_sigaction(args[0] as u32, args[1] as *const SigAction, args[2] as *mut SigAction),
        SYSCALL_SIGPROCMASK => sys_sigprocmask(args[0] as i32, args[1] as *const u64, args[2] as *mut u64),
        SYSCALL_SIGRETURN => sys_sigreturn(),
        SYSCALL_PAUSE => sys_pause(),
        SYSCALL_ALARM => sys_alarm(args[0] as u32),

        // 权限相关系统调用
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_SETEUID => sys_seteuid(args[0] as u32),
        SYSCALL_SETEGID => sys_setegid(args[0] as u32),

        // 动态链接相关系统调用
        SYSCALL_DLOPEN => sys_dlopen(args[0] as *const u8, args[1] as i32),
        SYSCALL_DLSYM => sys_dlsym(args[0], args[1] as *const u8),
        SYSCALL_DLCLOSE => sys_dlclose(args[0]),

        // 内存管理系统调用
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_SBRK => sys_sbrk(args[0] as isize),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2] as i32, 0, -1, 0),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),

        _ => {
            println!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
