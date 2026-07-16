use core::ffi::{c_char, c_int, c_long, c_uint, c_void};

pub const AF_UNIX: c_int = 1;
pub const SOCK_STREAM: c_int = 1;
pub const SOCK_NONBLOCK: c_int = 0x800;
pub const SOCK_CLOEXEC: c_int = 0x8_0000;
pub const SOL_SOCKET: c_int = 1;
pub const SO_PEERCRED: c_int = 17;
pub const SCM_RIGHTS: c_int = 1;
pub const O_RDONLY: c_int = 0;
pub const O_RDWR: c_int = 2;
pub const O_NONBLOCK: c_int = 0x800;
pub const O_CLOEXEC: c_int = 0x8_0000;
pub const POLLIN: i16 = 0x001;
pub const POLLOUT: i16 = 0x004;
pub const POLLERR: i16 = 0x008;
pub const POLLHUP: i16 = 0x010;
pub const EINTR: c_int = 4;
pub const EAGAIN: c_int = 11;
pub const EBUSY: c_int = 16;
pub const EBADF: c_int = 9;
pub const EINVAL: c_int = 22;
pub const EMFILE: c_int = 24;
pub const ENODEV: c_int = 19;
pub const ENOTSUP: c_int = 95;
pub const EPERM: c_int = 1;
pub const MSG_DONTWAIT: c_int = 0x40;
pub const MSG_NOSIGNAL: c_int = 0x4000;
pub const CLOCK_MONOTONIC: c_int = 1;
pub const DRM_IOCTL_DROP_MASTER: usize = 0x641f;
pub const EVIOCREVOKE: usize = 0x4004_4591;

#[repr(C)]
pub struct SockaddrUn {
    pub family: u16,
    pub path: [u8; 108],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub returned: i16,
}

#[repr(C)]
pub struct Timespec {
    pub seconds: c_long,
    pub nanoseconds: c_long,
}

#[repr(C)]
pub struct Ucred {
    pub pid: c_int,
    pub uid: c_uint,
    pub gid: c_uint,
}

#[repr(C)]
pub struct Iovec {
    pub base: *mut c_void,
    pub length: usize,
}

#[repr(C)]
pub struct MessageHeader {
    pub name: *mut c_void,
    pub name_length: u32,
    pub iov: *mut Iovec,
    pub iov_length: usize,
    pub control: *mut c_void,
    pub control_length: usize,
    pub flags: c_int,
}

#[repr(C)]
pub struct ControlHeader {
    pub length: usize,
    pub level: c_int,
    pub kind: c_int,
}

const _: () = assert!(core::mem::size_of::<Ucred>() == 12);
const _: () = assert!(core::mem::size_of::<ControlHeader>() == 16);

unsafe extern "C" {
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn bind(fd: c_int, address: *const SockaddrUn, length: u32) -> c_int;
    pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    pub fn accept4(fd: c_int, address: *mut c_void, length: *mut u32, flags: c_int) -> c_int;
    pub fn getsockopt(
        fd: c_int,
        level: c_int,
        option: c_int,
        value: *mut c_void,
        length: *mut u32,
    ) -> c_int;
    pub fn sendmsg(fd: c_int, message: *const MessageHeader, flags: c_int) -> isize;
    pub fn open(path: *const c_char, flags: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, output: *mut c_void, length: usize) -> isize;
    pub fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
    pub fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: usize, argument: *mut c_void) -> c_int;
    pub fn unlink(path: *const c_char) -> c_int;
    pub fn chmod(path: *const c_char, mode: c_uint) -> c_int;
    pub fn clock_gettime(clock: c_int, value: *mut Timespec) -> c_int;
    pub fn malloc(size: usize) -> *mut c_void;
    pub fn realloc(pointer: *mut c_void, size: usize) -> *mut c_void;
    pub fn free(pointer: *mut c_void);
    pub fn __errno_location() -> *mut c_int;
    pub fn _exit(status: c_int) -> !;
}

pub fn errno() -> c_int {
    // SAFETY: musl 为当前单线程 broker 暴露有效的 thread-local errno。
    unsafe { *__errno_location() }
}

pub fn monotonic_milliseconds() -> Result<u64, ()> {
    let mut value = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    if unsafe { clock_gettime(CLOCK_MONOTONIC, &mut value) } != 0 || value.seconds < 0 {
        return Err(());
    }
    Ok((value.seconds as u64)
        .saturating_mul(1_000)
        .saturating_add(value.nanoseconds as u64 / 1_000_000))
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
            break;
        }
    }
}
