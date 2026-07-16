use core::ffi::{c_char, c_int, c_uint, c_void};

pub const AF_UNIX: c_int = 1;
pub const SOCK_STREAM: c_int = 1;
pub const SOCK_NONBLOCK: c_int = 0x800;
pub const SOCK_CLOEXEC: c_int = 0x80000;
pub const F_SETFD: c_int = 2;
pub const SIGKILL: c_int = 9;
pub const EINTR: c_int = 4;
pub const ECHILD: c_int = 10;
pub const ESRCH: c_int = 3;
pub const CLOCK_MONOTONIC: c_int = 1;

#[repr(C)]
pub struct SockaddrUn {
    pub family: u16,
    pub path: [u8; 108],
}

#[repr(C)]
pub struct Timespec {
    pub seconds: i64,
    pub nanoseconds: i64,
}

unsafe extern "C" {
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn bind(fd: c_int, address: *const SockaddrUn, length: u32) -> c_int;
    pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    pub fn chmod(path: *const c_char, mode: c_uint) -> c_int;
    pub fn chown(path: *const c_char, owner: c_uint, group: c_uint) -> c_int;
    pub fn unlink(path: *const c_char) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn fork() -> c_int;
    pub fn getpid() -> c_int;
    pub fn execv(path: *const c_char, arguments: *const *const c_char) -> c_int;
    pub fn dup2(old: c_int, new: c_int) -> c_int;
    pub fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    pub fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    pub fn setgroups(count: usize, groups: *const c_uint) -> c_int;
    pub fn setgid(gid: c_uint) -> c_int;
    pub fn setuid(uid: c_uint) -> c_int;
    pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    pub fn kill(pid: c_int, signal: c_int) -> c_int;
    pub fn clock_gettime(clock: c_int, value: *mut Timespec) -> c_int;
    pub fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
    pub fn __errno_location() -> *mut c_int;
    pub fn _exit(status: c_int) -> !;
}

pub fn errno() -> c_int {
    unsafe { *__errno_location() }
}

pub fn write_stderr(message: &[u8]) {
    let mut written = 0;
    while written < message.len() {
        let count = unsafe {
            write(
                2,
                message[written..].as_ptr().cast(),
                message.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
        } else if count < 0 && errno() == EINTR {
            continue;
        } else {
            return;
        }
    }
}
