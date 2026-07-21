use std::ffi::{c_char, c_int, c_void};

pub(crate) const O_RDWR: c_int = 2;
pub(crate) const O_NONBLOCK: c_int = 0x800;
pub(crate) const O_CLOEXEC: c_int = 0x80000;
pub(crate) const PROT_READ: c_int = 1;
pub(crate) const PROT_WRITE: c_int = 2;
pub(crate) const MAP_SHARED: c_int = 1;
pub(crate) const POLLIN: i16 = 1;
pub(crate) const POLLOUT: i16 = 4;
pub(crate) const POLLERR: i16 = 8;
pub(crate) const POLLHUP: i16 = 16;
pub(crate) const SOL_SOCKET: c_int = 1;
pub(crate) const SCM_RIGHTS: c_int = 1;
pub(crate) const MSG_CMSG_CLOEXEC: c_int = 0x4000_0000;
pub(crate) const MSG_CTRUNC: c_int = 0x8;
pub(crate) const PR_SET_PDEATHSIG: c_int = 1;
pub(crate) const ECHILD: c_int = 10;
pub(crate) const SIGKILL: c_int = 9;
pub(crate) const SIGTERM: c_int = 15;

const IOC_WRITE: usize = 1;
const IOC_READ: usize = 2;
const fn ioc(direction: usize, kind: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | kind << 8 | number
}
const fn drm_iowr(number: usize, size: usize) -> usize {
    ioc(IOC_READ | IOC_WRITE, b'd' as usize, number, size)
}

pub(crate) const DRM_IOCTL_SET_MASTER: usize = ioc(0, b'd' as usize, 0x1e, 0);
pub(crate) const DRM_IOCTL_DROP_MASTER: usize = ioc(0, b'd' as usize, 0x1f, 0);
pub(crate) const DRM_IOCTL_MODE_GETRESOURCES: usize = drm_iowr(0xa0, 64);
pub(crate) const DRM_IOCTL_MODE_SETCRTC: usize = drm_iowr(0xa2, 104);
pub(crate) const DRM_IOCTL_MODE_GETCONNECTOR: usize = drm_iowr(0xa7, 80);
pub(crate) const DRM_IOCTL_MODE_ADDFB: usize = drm_iowr(0xae, 28);
pub(crate) const DRM_IOCTL_MODE_DIRTYFB: usize = drm_iowr(0xb1, 24);
pub(crate) const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_iowr(0xb2, 32);
pub(crate) const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_iowr(0xb3, 16);
pub(crate) const DRM_IOCTL_MODE_DESTROY_DUMB: usize = drm_iowr(0xb4, 4);
pub(crate) const EVIOCGNAME_128: usize = ioc(IOC_READ, b'E' as usize, 0x06, 128);
pub(crate) const EVIOCGABS_X: usize = ioc(IOC_READ, b'E' as usize, 0x40, 24);
pub(crate) const EVIOCGABS_Y: usize = ioc(IOC_READ, b'E' as usize, 0x41, 24);
pub(crate) const EVIOCGRAB: usize = ioc(IOC_WRITE, b'E' as usize, 0x90, 4);
pub(crate) const TIOCGPTN: usize = 0x8004_5430;
pub(crate) const TIOCSPTLCK: usize = 0x4004_5431;
pub(crate) const TIOCSCTTY: usize = 0x540e;
pub(crate) const TIOCSWINSZ: usize = 0x5414;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct DrmMode {
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
pub(crate) struct DrmResources {
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
pub(crate) struct DrmConnector {
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
pub(crate) struct DrmCrtc {
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
pub(crate) struct DrmDumbCreate {
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
pub(crate) struct DrmDumbMap {
    pub handle: u32,
    pub padding: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct DrmFramebuffer {
    pub framebuffer_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

#[repr(C)]
pub(crate) struct DrmDirty {
    pub framebuffer_id: u32,
    pub flags: u32,
    pub color: u32,
    pub clip_count: u32,
    pub clips: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct DrmClip {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct InputEvent {
    pub seconds: i64,
    pub microseconds: i64,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct InputAbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
pub(crate) struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub returned: i16,
}

#[repr(C)]
pub(crate) struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[repr(C)]
pub(crate) struct IoVec {
    pub base: *mut c_void,
    pub len: usize,
}

#[repr(C)]
pub(crate) struct MsgHdr {
    pub name: *mut c_void,
    pub name_len: u32,
    pub iov: *mut IoVec,
    pub iov_len: usize,
    pub control: *mut c_void,
    pub control_len: usize,
    pub flags: c_int,
}

#[repr(C)]
pub(crate) struct CmsgHdr {
    pub len: usize,
    pub level: c_int,
    pub kind: c_int,
}

const _: () = assert!(size_of::<DrmMode>() == 68);
const _: () = assert!(align_of::<DrmMode>() == 4);
const _: () = assert!(size_of::<DrmResources>() == 64);
const _: () = assert!(size_of::<DrmConnector>() == 80);
const _: () = assert!(size_of::<DrmCrtc>() == 104);
const _: () = assert!(size_of::<DrmDumbCreate>() == 32);
const _: () = assert!(size_of::<DrmDumbMap>() == 16);
const _: () = assert!(size_of::<DrmFramebuffer>() == 28);
const _: () = assert!(size_of::<DrmDirty>() == 24);
const _: () = assert!(size_of::<InputEvent>() == 24);
const _: () = assert!(size_of::<InputAbsInfo>() == 24);
const _: () = assert!(size_of::<PollFd>() == 8);
const _: () = assert!(size_of::<WindowSize>() == 8);
const _: () = assert!(size_of::<MsgHdr>() == 56);
const _: () = assert!(size_of::<CmsgHdr>() == 16);
const _: () = assert!(DRM_IOCTL_DROP_MASTER == 0x0000_641f);

unsafe extern "C" {
    pub(crate) fn open(path: *const c_char, flags: c_int, mode: u32) -> c_int;
    pub(crate) fn ioctl(fd: c_int, request: usize, argument: *mut c_void) -> c_int;
    pub(crate) fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    pub(crate) fn munmap(address: *mut c_void, length: usize) -> c_int;
    pub(crate) fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub(crate) fn sendmsg(fd: c_int, message: *const MsgHdr, flags: c_int) -> isize;
    pub(crate) fn recvmsg(fd: c_int, message: *mut MsgHdr, flags: c_int) -> isize;
    pub(crate) fn fork() -> c_int;
    pub(crate) fn getppid() -> c_int;
    pub(crate) fn prctl(option: c_int, argument: c_int) -> c_int;
    pub(crate) fn kill(pid: c_int, signal: c_int) -> c_int;
    pub(crate) fn setsid() -> c_int;
}
