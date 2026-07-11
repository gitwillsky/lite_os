#![no_std]

//! LiteOS 用户态与内核共享的系统调用编号。

pub const SYSCALL_GETCWD: usize = 17;
pub const SYSCALL_WRITE: usize = 64;
pub const SYSCALL_EXIT: usize = 93;
pub const SYSCALL_NANOSLEEP: usize = 101;
pub const SYSCALL_CLOCK_GETTIME: usize = 113;
pub const SYSCALL_SCHED_YIELD: usize = 124;
pub const SYSCALL_SETUID: usize = 146;
pub const SYSCALL_GETPID: usize = 172;
pub const SYSCALL_GETTID: usize = 178;
pub const SYSCALL_BRK: usize = 214;
pub const SYSCALL_EXECVE: usize = 221;
