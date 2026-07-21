use core::ffi::{c_char, c_int, c_void};

pub const O_RDONLY: c_int = 0;
pub const O_RDWR: c_int = 2;
pub const O_NONBLOCK: c_int = 0x800;
pub const O_CLOEXEC: c_int = 0x80000;
pub const SEEK_SET: c_int = 0;
pub const SEEK_END: c_int = 2;
pub const PROT_READ: c_int = 1;
pub const PROT_WRITE: c_int = 2;
pub const MAP_SHARED: c_int = 1;
pub const MAP_PRIVATE: c_int = 2;
pub const MAP_ANONYMOUS: c_int = 0x20;
pub const POLLIN: i16 = 1;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const EINTR: c_int = 4;
pub const EAGAIN: c_int = 11;
pub const EPIPE: c_int = 32;
pub const ECHILD: c_int = 10;
pub const CLOCK_MONOTONIC: c_int = 1;
pub const AF_UNIX: c_int = 1;
pub const SOCK_STREAM: c_int = 1;
pub const F_SETFL: c_int = 4;
pub const PR_SET_PDEATHSIG: c_int = 1;
pub const SIGKILL: c_int = 9;

const IOC_WRITE: usize = 1;
const IOC_READ: usize = 2;
const fn ioc(direction: usize, kind: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | kind << 8 | number
}
const fn drm_iowr(number: usize, size: usize) -> usize {
    ioc(IOC_READ | IOC_WRITE, b'd' as usize, number, size)
}

// 仅保留客户端建 dumb buffer 所需的两个 ioctl；handle 所有权随 CREATE_SURFACE
// 转移给桌面，本进程绝不 DESTROY_DUMB，也不做 modeset/ADDFB/SETCRTC/DIRTYFB。
pub const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_iowr(0xb2, 32);
pub const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_iowr(0xb3, 16);
pub const TIOCGPTN: usize = 0x8004_5430;
pub const TIOCSPTLCK: usize = 0x4004_5431;
pub const TIOCSCTTY: usize = 0x540e;
pub const TIOCSWINSZ: usize = 0x5414;

#[repr(C)]
#[derive(Default)]
pub struct DrmDumbCreate {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct DrmDumbMap {
    pub handle: u32,
    pub padding: u32,
    pub offset: u64,
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub returned: i16,
}

#[repr(C)]
pub struct Timespec {
    pub seconds: i64,
    pub nanoseconds: i64,
}

#[repr(C)]
pub struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[repr(C)]
pub struct SockaddrUn {
    pub family: u16,
    pub path: [u8; 108],
}

const _: () = assert!(core::mem::size_of::<DrmDumbCreate>() == 32);
const _: () = assert!(core::mem::size_of::<DrmDumbMap>() == 16);
const _: () = assert!(core::mem::size_of::<SockaddrUn>() == 110);

unsafe extern "C" {
    pub static mut environ: *mut *const c_char;
    pub fn open(path: *const c_char, flags: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, output: *mut c_void, length: usize) -> isize;
    pub fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
    pub fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64;
    pub fn ioctl(fd: c_int, request: usize, argument: *mut c_void) -> c_int;
    pub fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    pub fn munmap(address: *mut c_void, length: usize) -> c_int;
    pub fn calloc(count: usize, size: usize) -> *mut c_void;
    pub fn free(pointer: *mut c_void);
    pub fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub fn clock_gettime(clock: c_int, value: *mut Timespec) -> c_int;
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn connect(fd: c_int, address: *const SockaddrUn, length: u32) -> c_int;
    pub fn fcntl(fd: c_int, command: c_int, argument: c_int) -> c_int;
    pub fn fork() -> c_int;
    pub fn getpid() -> c_int;
    pub fn getppid() -> c_int;
    pub fn prctl(option: c_int, argument: c_int) -> c_int;
    pub fn kill(pid: c_int, signal: c_int) -> c_int;
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
    pub fn _exit(status: c_int) -> !;
    pub fn __errno_location() -> *mut c_int;
}

pub fn errno() -> c_int {
    // SAFETY: musl 为调用线程暴露唯一 thread-local errno 指针。
    unsafe { *__errno_location() }
}

pub fn monotonic_milliseconds() -> u64 {
    let mut value = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: value 在调用期间始终指向可写的 `timespec`。
    if unsafe { clock_gettime(CLOCK_MONOTONIC, &mut value) } != 0 {
        return 0;
    }
    (value.seconds as u64)
        .saturating_mul(1_000)
        .saturating_add(value.nanoseconds as u64 / 1_000_000)
}

pub const fn c_str(bytes: &'static [u8]) -> *const c_char {
    bytes.as_ptr().cast()
}

/// 把整个常规文件读进一块匿名 mmap，返回（映射指针，字节数）。
///
/// 所有权归调用方：随进程退出由内核回收，或用 `munmap` 提前释放。文件大小经
/// `lseek(SEEK_END)` 探测（常规文件可定位，见 syscall 矩阵 62 号）；open /
/// lseek / mmap / read 任一失败返回 `None`，调用方按启动失败处理。
pub fn read_file(path: &'static [u8]) -> Option<(*mut c_void, usize)> {
    // SAFETY: path 为 NUL 结尾静态字节串；只读打开。
    let fd = unsafe { open(c_str(path), O_RDONLY | O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let result = read_fd(fd);
    // SAFETY: fd 为本函数打开的描述符。
    unsafe { close(fd) };
    result
}

/// `read_file` 的 fd 部分：量尺寸、建映射、循环读满。
fn read_fd(fd: c_int) -> Option<(*mut c_void, usize)> {
    // SAFETY: lseek 不触碰缓冲区。
    let size = unsafe { lseek(fd, 0, SEEK_END) };
    if size <= 0 || unsafe { lseek(fd, 0, SEEK_SET) } != 0 {
        return None;
    }
    let size = usize::try_from(size).ok()?;
    // SAFETY: 匿名映射不触碰 fd；失败返回 MAP_FAILED（usize::MAX）。
    let pointer = unsafe {
        mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if pointer as usize == usize::MAX {
        return None;
    }
    let mut total = 0usize;
    while total < size {
        // SAFETY: 映射 size 字节可写，[total, size) 区间在本次 read 期间有效。
        let count = unsafe { read(fd, (pointer as *mut u8).add(total).cast(), size - total) };
        if count > 0 {
            total += count as usize;
        } else if count < 0 && errno() == EINTR {
            continue;
        } else {
            // SAFETY: 映射由本函数持有，此后不再访问。
            unsafe { munmap(pointer, size) };
            return None;
        }
    }
    Some((pointer, size))
}
