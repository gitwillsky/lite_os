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
#[cfg(target_os = "linux")]
pub(crate) const MSG_CMSG_CLOEXEC: c_int = 0x4000_0000;
#[cfg(target_os = "linux")]
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
pub(crate) const DRM_IOCTL_GEM_CLOSE: usize = ioc(IOC_WRITE, b'd' as usize, 0x09, 8);
pub(crate) const DRM_IOCTL_MODE_GETRESOURCES: usize = drm_iowr(0xa0, 64);
pub(crate) const DRM_IOCTL_MODE_SETCRTC: usize = drm_iowr(0xa2, 104);
pub(crate) const DRM_IOCTL_MODE_GETCONNECTOR: usize = drm_iowr(0xa7, 80);
pub(crate) const DRM_IOCTL_MODE_ADDFB: usize = drm_iowr(0xae, 28);
pub(crate) const DRM_IOCTL_MODE_RMFB: usize = drm_iowr(0xaf, 4);
pub(crate) const DRM_IOCTL_MODE_PAGE_FLIP: usize = drm_iowr(0xb0, 24);
pub(crate) const DRM_IOCTL_MODE_DIRTYFB: usize = drm_iowr(0xb1, 24);
pub(crate) const DRM_IOCTL_MODE_CREATE_DUMB: usize = drm_iowr(0xb2, 32);
pub(crate) const DRM_IOCTL_MODE_MAP_DUMB: usize = drm_iowr(0xb3, 16);
pub(crate) const DRM_IOCTL_MODE_DESTROY_DUMB: usize = drm_iowr(0xb4, 4);
pub(crate) const DRM_IOCTL_MODE_CURSOR2: usize = drm_iowr(0xbb, 36);
pub(crate) const DRM_IOCTL_VIRTGPU_MAP: usize = drm_iowr(0x41, 16);
pub(crate) const DRM_IOCTL_VIRTGPU_EXECBUFFER: usize = drm_iowr(0x42, 64);
pub(crate) const DRM_IOCTL_VIRTGPU_GETPARAM: usize = drm_iowr(0x43, 16);
pub(crate) const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: usize = drm_iowr(0x44, 56);
pub(crate) const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: usize = drm_iowr(0x45, 16);
pub(crate) const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: usize = drm_iowr(0x46, 44);
pub(crate) const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: usize = drm_iowr(0x47, 44);
pub(crate) const DRM_IOCTL_VIRTGPU_WAIT: usize = drm_iowr(0x48, 8);
pub(crate) const DRM_IOCTL_VIRTGPU_GET_CAPS: usize = drm_iowr(0x49, 24);
pub(crate) const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: usize = drm_iowr(0x4b, 16);
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
pub(crate) struct DrmPageFlip {
    pub crtc_id: u32,
    pub framebuffer_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct DrmCursor2 {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
    pub hot_x: u32,
    pub hot_y: u32,
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
pub(crate) struct VirtGpuMap {
    pub offset: u64,
    pub handle: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuExecBuffer {
    pub flags: u32,
    pub size: u32,
    pub command: u64,
    pub bo_handles: u64,
    pub num_bo_handles: u32,
    pub fence_fd: i32,
    pub ring_index: u32,
    pub syncobj_stride: u32,
    pub input_syncobj_count: u32,
    pub output_syncobj_count: u32,
    pub input_syncobjs: u64,
    pub output_syncobjs: u64,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuGetParam {
    pub param: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuResourceCreate {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub samples: u32,
    pub flags: u32,
    pub bo_handle: u32,
    pub resource_handle: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuResourceInfo {
    pub bo_handle: u32,
    pub resource_handle: u32,
    pub size: u32,
    pub blob_memory: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct VirtGpuBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuTransfer3d {
    pub bo_handle: u32,
    pub region: VirtGpuBox,
    pub level: u32,
    pub offset: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuWait {
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuGetCaps {
    pub capset_id: u32,
    pub capset_version: u32,
    pub address: u64,
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct VirtGpuContextParameter {
    pub parameter: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct VirtGpuContextInit {
    pub parameter_count: u32,
    pub padding: u32,
    pub parameters: u64,
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

#[cfg(target_os = "linux")]
#[repr(C)]
pub(crate) struct SockAddrNl {
    pub family: u16,
    pub padding: u16,
    pub port_id: u32,
    pub groups: u32,
}

const _: () = assert!(size_of::<DrmMode>() == 68);
const _: () = assert!(align_of::<DrmMode>() == 4);
const _: () = assert!(size_of::<DrmResources>() == 64);
const _: () = assert!(size_of::<DrmConnector>() == 80);
const _: () = assert!(size_of::<DrmCrtc>() == 104);
const _: () = assert!(size_of::<DrmCursor2>() == 36);
const _: () = assert!(size_of::<DrmDumbCreate>() == 32);
const _: () = assert!(size_of::<DrmDumbMap>() == 16);
const _: () = assert!(size_of::<VirtGpuMap>() == 16);
const _: () = assert!(size_of::<VirtGpuExecBuffer>() == 64);
const _: () = assert!(size_of::<VirtGpuGetParam>() == 16);
const _: () = assert!(size_of::<VirtGpuResourceCreate>() == 56);
const _: () = assert!(std::mem::offset_of!(VirtGpuResourceCreate, size) == 48);
const _: () = assert!(size_of::<VirtGpuResourceInfo>() == 16);
const _: () = assert!(size_of::<VirtGpuTransfer3d>() == 44);
const _: () = assert!(size_of::<VirtGpuWait>() == 8);
const _: () = assert!(size_of::<DrmGemClose>() == 8);
const _: () = assert!(size_of::<VirtGpuGetCaps>() == 24);
const _: () = assert!(size_of::<VirtGpuContextParameter>() == 16);
const _: () = assert!(size_of::<VirtGpuContextInit>() == 16);
const _: () = assert!(size_of::<DrmFramebuffer>() == 28);
const _: () = assert!(size_of::<DrmDirty>() == 24);
const _: () = assert!(size_of::<InputEvent>() == 24);
const _: () = assert!(size_of::<InputAbsInfo>() == 24);
const _: () = assert!(size_of::<PollFd>() == 8);
const _: () = assert!(size_of::<WindowSize>() == 8);
const _: () = assert!(size_of::<MsgHdr>() == 56);
const _: () = assert!(size_of::<CmsgHdr>() == 16);
#[cfg(target_os = "linux")]
const _: () = assert!(size_of::<SockAddrNl>() == 12);
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
    pub(crate) fn ftruncate(fd: c_int, length: i64) -> c_int;
    #[cfg(target_os = "linux")]
    pub(crate) fn fcntl(fd: c_int, command: c_int, argument: c_int) -> c_int;
    #[cfg(target_os = "linux")]
    pub(crate) fn syscall(number: isize, ...) -> isize;
    pub(crate) fn poll(descriptors: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    pub(crate) fn sendmsg(fd: c_int, message: *const MsgHdr, flags: c_int) -> isize;
    pub(crate) fn recvmsg(fd: c_int, message: *mut MsgHdr, flags: c_int) -> isize;
    #[cfg(target_os = "linux")]
    pub(crate) fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    #[cfg(target_os = "linux")]
    pub(crate) fn bind(fd: c_int, address: *const c_void, length: u32) -> c_int;
    #[cfg(target_os = "linux")]
    pub(crate) fn recv(fd: c_int, bytes: *mut c_void, length: usize, flags: c_int) -> isize;
    pub(crate) fn dup2(old_fd: c_int, new_fd: c_int) -> c_int;
    #[cfg(target_os = "linux")]
    pub(crate) fn getsockopt(
        fd: c_int,
        level: c_int,
        option: c_int,
        value: *mut c_void,
        length: *mut u32,
    ) -> c_int;
    pub(crate) fn fork() -> c_int;
    pub(crate) fn getppid() -> c_int;
    pub(crate) fn prctl(option: c_int, argument: c_int) -> c_int;
    pub(crate) fn kill(pid: c_int, signal: c_int) -> c_int;
    pub(crate) fn setsid() -> c_int;
}
