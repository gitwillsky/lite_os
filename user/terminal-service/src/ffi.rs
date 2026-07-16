use core::ffi::{c_char, c_int, c_void};

pub const O_RDWR: c_int = 2;
pub const O_NONBLOCK: c_int = 0x800;
pub const O_CLOEXEC: c_int = 0x80000;
pub const F_SETFL: c_int = 4;
pub const POLLIN: i16 = 1;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const AF_UNIX: c_int = 1;
pub const SOCK_STREAM: c_int = 1;
pub const SOCK_CLOEXEC: c_int = 0x80000;
pub const MSG_NOSIGNAL: c_int = 0x4000;
pub const EINTR: c_int = 4;
pub const EAGAIN: c_int = 11;
pub const ECHILD: c_int = 10;
pub const SIGKILL: c_int = 9;
pub const CLOCK_MONOTONIC: c_int = 1;
pub const PR_SET_PDEATHSIG: c_int = 1;
pub const TIOCGPTN: usize = 0x8004_5430;
pub const TIOCSPTLCK: usize = 0x4004_5431;
pub const TIOCSCTTY: usize = 0x540e;
pub const TIOCSWINSZ: usize = 0x5414;

#[repr(C)]
pub struct SockaddrUn {
    pub family: u16,
    pub path: [u8; 108],
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub returned: i16,
}

#[repr(C)]
pub struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[repr(C)]
pub struct Timespec {
    pub seconds: i64,
    pub nanoseconds: i64,
}

unsafe extern "C" {
    pub static mut environ: *mut *const c_char;
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn connect(fd: c_int, address: *const SockaddrUn, length: u32) -> c_int;
    pub fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    pub fn open(path: *const c_char, flags: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, output: *mut c_void, length: usize) -> isize;
    pub fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
    pub fn send(fd: c_int, input: *const c_void, length: usize, flags: c_int) -> isize;
    pub fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: usize, argument: *mut c_void) -> c_int;
    pub fn fork() -> c_int;
    pub fn getpid() -> c_int;
    pub fn getppid() -> c_int;
    pub fn setsid() -> c_int;
    pub fn dup2(old: c_int, new: c_int) -> c_int;
    pub fn chdir(path: *const c_char) -> c_int;
    pub fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    pub fn execve(
        path: *const c_char,
        arguments: *const *const c_char,
        environment: *const *const c_char,
    ) -> c_int;
    pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    pub fn kill(pid: c_int, signal: c_int) -> c_int;
    pub fn prctl(option: c_int, argument: c_int, ...) -> c_int;
    pub fn clock_gettime(clock: c_int, value: *mut Timespec) -> c_int;
    pub fn malloc(size: usize) -> *mut c_void;
    pub fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    pub fn calloc(count: usize, size: usize) -> *mut c_void;
    pub fn free(pointer: *mut c_void);
    pub fn __errno_location() -> *mut c_int;
    pub fn _exit(status: c_int) -> !;
}

pub fn errno() -> c_int {
    unsafe { *__errno_location() }
}

pub fn monotonic_milliseconds() -> Result<u64, ()> {
    let mut value = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    if unsafe { clock_gettime(CLOCK_MONOTONIC, &mut value) } != 0
        || value.seconds < 0
        || value.nanoseconds < 0
    {
        return Err(());
    }
    (value.seconds as u64)
        .checked_mul(1_000)
        .and_then(|seconds| seconds.checked_add(value.nanoseconds as u64 / 1_000_000))
        .ok_or(())
}

pub const fn c_str(bytes: &'static [u8]) -> *const c_char {
    bytes.as_ptr().cast()
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
