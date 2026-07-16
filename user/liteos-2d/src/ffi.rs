use core::ffi::{c_char, c_int, c_void};

pub const O_NONBLOCK: c_int = 0x800;
pub const O_CLOEXEC: c_int = 0x80000;
pub const EFD_CLOEXEC: c_int = O_CLOEXEC;
pub const PROT_READ: c_int = 1;
pub const PROT_WRITE: c_int = 2;
pub const MAP_SHARED: c_int = 1;
pub const POLLIN: i16 = 1;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const EINTR: c_int = 4;
pub const EAGAIN: c_int = 11;
pub const ENOMEM: c_int = 12;
pub const EBUSY: c_int = 16;
pub const EINVAL: c_int = 22;
pub const CLOCK_MONOTONIC: c_int = 1;
pub const AF_NETLINK: u16 = 16;
pub const SOCK_DGRAM: c_int = 2;
pub const NETLINK_KOBJECT_UEVENT: c_int = 15;
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 1;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 2;

const IOC_WRITE: usize = 1;
const IOC_READ: usize = 2;
const fn ioc(direction: usize, kind: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | kind << 8 | number
}
const fn drm_iowr(number: usize, size: usize) -> usize {
    ioc(IOC_READ | IOC_WRITE, b'd' as usize, number, size)
}

pub const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_iowr(0xb2, 32);
pub const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_iowr(0xb3, 16);
pub const DRM_IOCTL_MODE_DESTROY_DUMB: usize = drm_iowr(0xb4, 4);
pub const EVIOCGNAME_128: usize = ioc(IOC_READ, b'E' as usize, 0x06, 128);
pub const EVIOCGABS_X: usize = ioc(IOC_READ, b'E' as usize, 0x40, 24);
pub const EVIOCGABS_Y: usize = ioc(IOC_READ, b'E' as usize, 0x41, 24);
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
pub struct LibDrmResources {
    pub framebuffer_count: i32,
    pub framebuffer_ids: *mut u32,
    pub crtc_count: i32,
    pub crtc_ids: *mut u32,
    pub connector_count: i32,
    pub connector_ids: *mut u32,
    pub encoder_count: i32,
    pub encoder_ids: *mut u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
pub struct LibDrmConnector {
    pub connector_id: u32,
    pub encoder_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub subpixel: u32,
    pub mode_count: i32,
    pub modes: *mut DrmMode,
    pub property_count: i32,
    pub properties: *mut u32,
    pub property_values: *mut u64,
    pub encoder_count: i32,
    pub encoders: *mut u32,
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
#[derive(Clone, Copy, Default)]
pub struct InputAbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
pub struct Timespec {
    pub seconds: i64,
    pub nanoseconds: i64,
}

#[repr(C)]
pub struct SockaddrNl {
    pub family: u16,
    pub padding: u16,
    pub port_id: u32,
    pub groups: u32,
}

pub type Pthread = *mut c_void;

const _: () = assert!(core::mem::size_of::<DrmMode>() == 68);
const _: () = assert!(core::mem::size_of::<LibDrmResources>() == 80);
const _: () = assert!(core::mem::size_of::<LibDrmConnector>() == 88);
const _: () = assert!(core::mem::size_of::<DrmDumbCreate>() == 32);
const _: () = assert!(core::mem::size_of::<DrmClip>() == 8);
const _: () = assert!(core::mem::size_of::<InputEvent>() == 24);

unsafe extern "C" {
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
    pub fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub fn clock_gettime(clock: c_int, value: *mut Timespec) -> c_int;
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn bind(fd: c_int, address: *const SockaddrNl, length: u32) -> c_int;
    pub fn eventfd(initial: u32, flags: c_int) -> c_int;
    pub fn pthread_create(
        thread: *mut Pthread,
        attributes: *const c_void,
        start: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        argument: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: Pthread, result: *mut *mut c_void) -> c_int;
    pub fn __errno_location() -> *mut c_int;
    pub fn _exit(status: c_int) -> !;
    pub fn drmIoctl(fd: c_int, request: usize, argument: *mut c_void) -> c_int;
    pub fn drmModeGetResources(fd: c_int) -> *mut LibDrmResources;
    pub fn drmModeFreeResources(resources: *mut LibDrmResources);
    pub fn drmModeGetConnector(fd: c_int, connector_id: u32) -> *mut LibDrmConnector;
    pub fn drmModeFreeConnector(connector: *mut LibDrmConnector);
    pub fn drmModeAddFB(
        fd: c_int,
        width: u32,
        height: u32,
        depth: u8,
        bits_per_pixel: u8,
        pitch: u32,
        handle: u32,
        framebuffer_id: *mut u32,
    ) -> c_int;
    pub fn drmModeRmFB(fd: c_int, framebuffer_id: u32) -> c_int;
    pub fn drmModeDirtyFB(
        fd: c_int,
        framebuffer_id: u32,
        clips: *mut DrmClip,
        clip_count: u32,
    ) -> c_int;
    pub fn drmModeSetCrtc(
        fd: c_int,
        crtc_id: u32,
        framebuffer_id: u32,
        x: u32,
        y: u32,
        connectors: *mut u32,
        connector_count: c_int,
        mode: *mut DrmMode,
    ) -> c_int;
    pub fn drmModePageFlip(
        fd: c_int,
        crtc_id: u32,
        framebuffer_id: u32,
        flags: u32,
        user_data: *mut c_void,
    ) -> c_int;
}

pub fn errno() -> c_int {
    unsafe { *__errno_location() }
}

pub fn monotonic_milliseconds() -> u64 {
    let mut value = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
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
