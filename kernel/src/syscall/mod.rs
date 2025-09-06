mod errno;
mod fs;
mod futex;
pub mod graphics;
mod memory;
mod process;
mod signal;
mod timer;

use crate::memory::page_table::{translated_byte_buffer, translated_ref_mut};
use crate::task::{current_user_token, current_task};
use fs::*;
use futex::*;
use memory::*;
use process::*;
use signal::*;
use timer::*;

pub use signal::sys_rt_sigreturn;

// Core process syscalls
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_EXIT_GROUP: usize = 94;
const SYSCALL_SCHED_YIELD: usize = 124;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETPPID: usize = 173;
const SYSCALL_GETTID: usize = 178;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_EXECVE: usize = 221;
const SYSCALL_WAIT4: usize = 260;
const SYSCALL_WAITID: usize = 95;
const SYSCALL_SET_TID_ADDRESS: usize = 96;

// File system syscalls
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_NEWFSTATAT: usize = 79;
const SYSCALL_GETCWD: usize = 17;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_FCHMOD: usize = 52;
const SYSCALL_FCHOWNAT: usize = 54;
const SYSCALL_FACCESSAT: usize = 48;
const SYSCALL_PPOLL: usize = 73;

// Scheduling syscalls
const SYSCALL_SETPRIORITY: usize = 140;
const SYSCALL_GETPRIORITY: usize = 141;
const SYSCALL_SCHED_SETSCHEDULER: usize = 119;
const SYSCALL_SCHED_GETSCHEDULER: usize = 120;

// Signal syscalls
const SYSCALL_KILL: usize = 129;
const SYSCALL_RT_SIGACTION: usize = 134;
const SYSCALL_RT_SIGPROCMASK: usize = 135;
const SYSCALL_RT_SIGRETURN: usize = 139;
const SYSCALL_RT_SIGSUSPEND: usize = 133;

// User/Group ID syscalls
const SYSCALL_GETUID: usize = 174;
const SYSCALL_GETGID: usize = 176;
const SYSCALL_GETEUID: usize = 175;
const SYSCALL_GETEGID: usize = 177;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETGID: usize = 144;


// Memory management syscalls
const SYSCALL_BRK: usize = 214;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_MSYNC: usize = 227;
const SYSCALL_MLOCK: usize = 228;
const SYSCALL_MUNLOCK: usize = 229;
const SYSCALL_MLOCKALL: usize = 230;
const SYSCALL_MUNLOCKALL: usize = 231;
const SYSCALL_MREMAP: usize = 216;
const SYSCALL_MADVISE: usize = 233;

// Thread synchronization
const SYSCALL_FUTEX: usize = 98;
const SYSCALL_SET_ROBUST_LIST: usize = 99;
const SYSCALL_GET_ROBUST_LIST: usize = 100;


// Time syscalls
const SYSCALL_NANOSLEEP: usize = 101;
const SYSCALL_CLOCK_GETTIME: usize = 113;
const SYSCALL_CLOCK_SETTIME: usize = 112;
const SYSCALL_CLOCK_GETRES: usize = 114;
const SYSCALL_CLOCK_NANOSLEEP: usize = 115;
const SYSCALL_TIMER_CREATE: usize = 107;
const SYSCALL_TIMER_SETTIME: usize = 110;
const SYSCALL_TIMER_DELETE: usize = 111;


pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    match syscall_id {
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit(args[0] as i32),
        SYSCALL_SCHED_YIELD => sys_yield(),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_GETPPID => {
            if let Some(current) = current_task() {
                current.parent().map(|p| p.pid()).unwrap_or(0) as isize
            } else {
                -1
            }
        },
        SYSCALL_GETTID => sys_gettid(),
        SYSCALL_CLONE => {
            // Basic clone implementation - treat as fork if no stack
            if args[1] == 0 {
                sys_fork()
            } else {
                sys_thread_create(args[1], args[1], 0)
            }
        },
        SYSCALL_EXECVE => sys_execve(
            args[0] as *const u8,
            args[1] as *const *const u8,
            args[2] as *const *const u8,
        ),
        SYSCALL_WAIT4 => sys_wait_pid(args[0] as isize, args[1] as *mut i32),
        SYSCALL_WAITID => sys_wait_pid(-1, core::ptr::null_mut()),
        SYSCALL_SET_TID_ADDRESS => sys_gettid(),

        // File system syscalls
        SYSCALL_OPENAT => sys_open(args[1] as *const u8, args[2] as u32),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as isize, args[2]),
        SYSCALL_PIPE2 => sys_pipe(args[0] as *mut i32),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup(args[0]),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1] as i32, args[2]),
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut u8),
        SYSCALL_NEWFSTATAT => sys_newfstatat(args[0] as i32, args[1] as *const u8, args[2] as *mut u8),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as i32, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as i32, args[1] as *const u8, args[2] as i32),
        SYSCALL_FCHMOD => sys_fchmod(args[0], args[1] as u32),
        SYSCALL_FCHOWNAT => sys_fchownat(args[0] as i32, args[1] as *const u8, args[2] as u32, args[3] as u32, args[4] as i32),
        SYSCALL_FACCESSAT => sys_faccessat(args[0] as i32, args[1] as *const u8, args[2] as i32, args[3] as i32),
        SYSCALL_PPOLL => sys_ppoll(args[0] as *mut u8, args[1], args[2] as *const u8, args[3] as *const u64),

        // Scheduling syscalls
        SYSCALL_SETPRIORITY => sys_setpriority(args[0] as i32, args[1] as i32, args[2] as i32),
        SYSCALL_GETPRIORITY => sys_getpriority(args[0] as i32, args[1] as i32),
        SYSCALL_SCHED_SETSCHEDULER => {
            sys_sched_setscheduler(args[0] as i32, args[1] as i32, args[2] as *const u8)
        }
        SYSCALL_SCHED_GETSCHEDULER => sys_sched_getscheduler(args[0] as i32),

        // Signal syscalls
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_RT_SIGACTION => sys_rt_sigaction(
            args[0] as u32,
            args[1] as *const SigAction,
            args[2] as *mut SigAction,
        ),
        SYSCALL_RT_SIGPROCMASK => {
            sys_rt_sigprocmask(args[0] as i32, args[1] as *const u64, args[2] as *mut u64)
        }
        SYSCALL_RT_SIGRETURN => sys_rt_sigreturn(),
        SYSCALL_RT_SIGSUSPEND => sys_rt_sigsuspend(args[0] as *const u64),

        // User/Group ID syscalls
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),


        // Memory management syscalls
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2] as i32, args[3] as i32, args[4] as i32, args[5]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2] as i32),
        SYSCALL_MSYNC => sys_msync(args[0], args[1], args[2] as i32),
        SYSCALL_MLOCK => sys_mlock(args[0], args[1]),
        SYSCALL_MUNLOCK => sys_munlock(args[0], args[1]),
        SYSCALL_MLOCKALL => sys_mlockall(args[0] as i32),
        SYSCALL_MUNLOCKALL => sys_munlockall(),
        SYSCALL_MREMAP => sys_mremap(args[0], args[1], args[2]),
        SYSCALL_MADVISE => sys_madvise(args[0], args[1], args[2] as i32),

        // Thread synchronization
        SYSCALL_FUTEX => sys_futex(args[0] as *mut i32, args[1] as i32, args[2] as i32),
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0] as *mut u8, args[1]),
        SYSCALL_GET_ROBUST_LIST => sys_get_robust_list(args[0] as i32, args[1] as *mut *mut u8),


        // Time syscalls
        SYSCALL_NANOSLEEP => sys_nanosleep(args[0] as *const TimeSpec, args[1] as *mut TimeSpec),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as i32, args[1] as *mut TimeSpec),
        SYSCALL_CLOCK_SETTIME => sys_clock_settime(args[0] as i32, args[1] as *const TimeSpec),
        SYSCALL_CLOCK_GETRES => sys_clock_getres(args[0] as i32, args[1] as *mut TimeSpec),
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(args[0] as i32, args[1] as i32, args[2] as *const TimeSpec),
        SYSCALL_TIMER_CREATE => sys_timer_create(args[0] as i32, args[1] as *mut u8, args[2] as *mut i32),
        SYSCALL_TIMER_SETTIME => sys_timer_settime(args[0] as i32, args[1] as i32, args[2] as *const u8),
        SYSCALL_TIMER_DELETE => sys_timer_delete(args[0] as i32),



        _ => {
            println!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
