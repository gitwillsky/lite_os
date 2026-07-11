#![no_std]

//! LiteOS 用户态与内核共享的系统调用编号。

pub const SYSCALL_GETCWD: usize = 17;
pub const SYSCALL_DUP: usize = 23;
pub const SYSCALL_FCNTL: usize = 25;
pub const SYSCALL_CLOSE: usize = 57;
pub const SYSCALL_LSEEK: usize = 62;
pub const SYSCALL_READ: usize = 63;
pub const SYSCALL_WRITE: usize = 64;
pub const SYSCALL_EXIT: usize = 93;
pub const SYSCALL_NANOSLEEP: usize = 101;
pub const SYSCALL_SCHED_YIELD: usize = 124;
pub const SYSCALL_KILL: usize = 129;
pub const SYSCALL_RT_SIGRETURN: usize = 139;
pub const SYSCALL_SETUID: usize = 146;
pub const SYSCALL_GETPID: usize = 172;
pub const SYSCALL_GETTID: usize = 178;
pub const SYSCALL_BRK: usize = 214;
pub const SYSCALL_EXECVE: usize = 221;
