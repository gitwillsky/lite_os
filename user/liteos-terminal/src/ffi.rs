use core::ffi::{c_char, c_int, c_void};

pub const O_RDONLY: c_int = 0;
pub const O_RDWR: c_int = 2;
pub const O_NONBLOCK: c_int = 0x800;
pub const O_CLOEXEC: c_int = 0x80000;
pub const PROT_READ: c_int = 1;
pub const PROT_WRITE: c_int = 2;
pub const MAP_SHARED: c_int = 1;
pub const POLLIN: i16 = 1;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const EINTR: c_int = 4;
pub const EAGAIN: c_int = 11;
pub const EPIPE: c_int = 32;
pub const CLOCK_MONOTONIC: c_int = 1;
pub const AF_NETLINK: u16 = 16;
pub const SOCK_DGRAM: c_int = 2;
pub const NETLINK_KOBJECT_UEVENT: c_int = 15;

const IOC_WRITE: usize = 1;
const IOC_READ: usize = 2;
const fn ioc(direction: usize, kind: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | kind << 8 | number
}
const fn drm_iowr(number: usize, size: usize) -> usize {
    ioc(IOC_READ | IOC_WRITE, b'd' as usize, number, size)
}

pub const DRM_IOCTL_MODE_GETRESOURCES: usize = drm_iowr(0xa0, 64);
pub const DRM_IOCTL_MODE_SETCRTC: usize = drm_iowr(0xa2, 104);
pub const DRM_IOCTL_MODE_GETCONNECTOR: usize = drm_iowr(0xa7, 80);
pub const DRM_IOCTL_MODE_ADDFB: usize = drm_iowr(0xae, 28);
pub const DRM_IOCTL_MODE_RMFB: usize = drm_iowr(0xaf, 4);
pub const DRM_IOCTL_MODE_DIRTYFB: usize = drm_iowr(0xb1, 24);
pub const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_iowr(0xb2, 32);
pub const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_iowr(0xb3, 16);
pub const DRM_IOCTL_MODE_DESTROY_DUMB: usize = drm_iowr(0xb4, 4);
pub const TIOCGPTN: usize = 0x8004_5430;
pub const TIOCSPTLCK: usize = 0x4004_5431;
pub const TIOCSCTTY: usize = 0x540e;
pub const TIOCSWINSZ: usize = 0x5414;
pub const EVIOCGNAME_128: usize = ioc(IOC_READ, b'E' as usize, 0x06, 128);
pub const EVIOCGRAB: usize = ioc(IOC_WRITE, b'E' as usize, 0x90, 4);

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmMode {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub mode_type: u32,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
pub struct DrmResources {
    pub framebuffer_ids: u64,
    pub crtc_ids: u64,
    pub connector_ids: u64,
    pub encoder_ids: u64,
    pub framebuffer_count: u32,
    pub crtc_count: u32,
    pub connector_count: u32,
    pub encoder_count: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct DrmConnector {
    pub encoder_ids: u64,
    pub modes: u64,
    pub properties: u64,
    pub property_values: u64,
    pub mode_count: u32,
    pub property_count: u32,
    pub encoder_count: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub subpixel: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct DrmCrtc {
    pub connectors: u64,
    pub connector_count: u32,
    pub crtc_id: u32,
    pub framebuffer_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmMode,
}

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
#[derive(Default)]
pub struct DrmFramebuffer {
    pub framebuffer_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

#[repr(C)]
pub struct DrmDirty {
    pub framebuffer_id: u32,
    pub flags: u32,
    pub color: u32,
    pub clip_count: u32,
    pub clips: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmClip {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub returned: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub seconds: i64,
    pub microseconds: i64,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
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
pub struct SockaddrNl {
    pub family: u16,
    pub padding: u16,
    pub port_id: u32,
    pub groups: u32,
}

const _: () = assert!(core::mem::size_of::<DrmMode>() == 68);
const _: () = assert!(core::mem::size_of::<DrmResources>() == 64);
const _: () = assert!(core::mem::size_of::<DrmConnector>() == 80);
const _: () = assert!(core::mem::size_of::<DrmCrtc>() == 104);
const _: () = assert!(core::mem::size_of::<DrmDumbCreate>() == 32);
const _: () = assert!(core::mem::size_of::<DrmDirty>() == 24);
const _: () = assert!(core::mem::size_of::<InputEvent>() == 24);

unsafe extern "C" {
    pub static mut environ: *mut *const c_char;
    pub fn open(path: *const c_char, flags: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, output: *mut c_void, length: usize) -> isize;
    pub fn write(fd: c_int, input: *const c_void, length: usize) -> isize;
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
    pub fn bind(fd: c_int, address: *const SockaddrNl, length: u32) -> c_int;
    pub fn fork() -> c_int;
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
