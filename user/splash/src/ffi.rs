//! DRM 与 libc 绑定：从 console-session 复制 splash 所需子集，新增 `DROP_MASTER`。

use core::ffi::{c_char, c_int, c_void};

pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_CREAT: c_int = 0x40;
pub const O_TRUNC: c_int = 0x200;
pub const O_CLOEXEC: c_int = 0x80000;
pub const PROT_READ: c_int = 1;
pub const PROT_WRITE: c_int = 2;
pub const MAP_SHARED: c_int = 1;
pub const EINTR: c_int = 4;

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
pub const DRM_IOCTL_MODE_DIRTYFB: usize = drm_iowr(0xb1, 24);
pub const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_iowr(0xb2, 32);
pub const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_iowr(0xb3, 16);
/// 无参数释放 DRM master；之后 desktop 才能接管 modeset，本进程仍可 DIRTYFB。
pub const DRM_IOCTL_DROP_MASTER: usize = ioc(0, b'd' as usize, 0x1f, 0);

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

const _: () = assert!(core::mem::size_of::<DrmMode>() == 68);
const _: () = assert!(core::mem::size_of::<DrmResources>() == 64);
const _: () = assert!(core::mem::size_of::<DrmConnector>() == 80);
const _: () = assert!(core::mem::size_of::<DrmCrtc>() == 104);
const _: () = assert!(core::mem::size_of::<DrmDumbCreate>() == 32);
const _: () = assert!(core::mem::size_of::<DrmDirty>() == 24);
const _: () = assert!(DRM_IOCTL_DROP_MASTER == 0x0000_641F);

unsafe extern "C" {
    /// mode 仅在 flags 含 O_CREAT 时生效，其余调用传 0。
    pub fn open(path: *const c_char, flags: c_int, mode: u32) -> c_int;
    pub fn close(fd: c_int) -> c_int;
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
    pub fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub fn fork() -> c_int;
    pub fn getpid() -> c_int;
    pub fn _exit(status: c_int) -> !;
    pub fn __errno_location() -> *mut c_int;
}

pub fn errno() -> c_int {
    // SAFETY: musl 为调用线程暴露唯一 thread-local errno 指针。
    unsafe { *__errno_location() }
}

/// 带 EINTR 重试的 ioctl，返回最终 ioctl 返回值。
///
/// # Safety
/// `argument` 必须满足 `request` 对应内核 UAPI 结构体的读写要求，且在整个调用期间有效。
pub unsafe fn drm_ioctl(fd: c_int, request: usize, argument: *mut c_void) -> c_int {
    loop {
        // SAFETY: fd 与 argument 由调用者保证；EINTR 表示 syscall 未生效，重试是安全的。
        let result = unsafe { ioctl(fd, request, argument) };
        if result >= 0 || errno() != EINTR {
            return result;
        }
    }
}

pub const fn c_str(bytes: &'static [u8]) -> *const c_char {
    bytes.as_ptr().cast()
}
