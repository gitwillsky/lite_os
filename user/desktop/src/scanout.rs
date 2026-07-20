//! DRM master、modeset 与 scanout framebuffer。
//!
//! 桌面是 `/dev/dri/card0` 的唯一 open 者（首个 open 自动成为 DRM master）；
//! 客户端的 fd 是握手时经 `SCM_RIGHTS` 传出的 dup，共享同一 OFD / handle
//! namespace，因此桌面可直接 `MAP_DUMB` 客户端的 handle 读取像素。
//!
//! 单缓冲模型：合成直接画进 scanout fb，每帧用 `DIRTYFB` 提交 damage clip
//! （内核 clip 上限 32，超出时坍缩为单个 union clip）。mode 在会话内是常量，
//! 不处理 resize / hotplug。

use core::ffi::c_void;

use crate::ffi::{
    self, DrmClip, DrmConnector, DrmCrtc, DrmDirty, DrmDumbCreate, DrmDumbMap, DrmFramebuffer,
    DrmMode, DrmResources,
};

/// 内核 `DIRTYFB` 的 clip 上限。
const MAX_CLIPS: usize = 32;

/// 半开矩形 `{x1, y1, x2, y2}`，坐标为屏幕绝对坐标，允许负值（合成前裁剪）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub const fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    pub fn is_empty(self) -> bool {
        self.x2 <= self.x1 || self.y2 <= self.y1
    }

    pub fn intersect(self, other: Rect) -> Rect {
        Rect {
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
            x2: self.x2.min(other.x2),
            y2: self.y2.min(other.y2),
        }
    }

    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        (self.x1..self.x2).contains(&x) && (self.y1..self.y2).contains(&y)
    }

    pub fn width(self) -> i32 {
        self.x2 - self.x1
    }

    pub fn height(self) -> i32 {
        self.y2 - self.y1
    }
}

/// scanout fb 的可变像素视图。单线程合成器内使用；`row` 返回整行切片，
/// 调用方自行按 x 范围裁剪（范围越界会 panic，属编程错误）。
pub struct Frame {
    pixels: *mut u32,
    pitch: usize,
    width: usize,
    height: usize,
}

impl Frame {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn row(&mut self, y: usize) -> &mut [u32] {
        assert!(y < self.height);
        // SAFETY: pixels 指向 `pitch * height` 字节的 MAP_SHARED 映射，
        // 第 y 行 [pitch*y, pitch*y + width*4) 必在映射内；桌面单线程，
        // 同一时刻只有一个 Frame 存活，行切片互不重叠。
        unsafe {
            core::slice::from_raw_parts_mut(
                (self.pixels as *mut u8).add(y * self.pitch).cast::<u32>(),
                self.width,
            )
        }
    }
}

/// 屏幕 mode（会话内常量）。
#[derive(Clone, Copy)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
}

pub struct Scanout {
    fd: i32,
    framebuffer_id: u32,
    handle: u32,
    pixels: *mut u32,
    size: usize,
    pitch: usize,
    mode: Mode,
}

impl Scanout {
    /// 打开 card0（成为 DRM master），完成 modeset 并切到 scanout fb。
    ///
    /// 任一步骤失败即清理已申请的资源并返回 `Err(())`；`SETCRTC` 同步阻塞，
    /// `EINTR` 时重试。
    pub fn open() -> Result<Self, ()> {
        let fd = unsafe {
            ffi::open(
                ffi::c_str(b"/dev/dri/card0\0"),
                ffi::O_RDWR | ffi::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(());
        }
        let result = Self::modeset(fd);
        if result.is_err() {
            // SAFETY: fd 为本函数打开的有效描述符。
            unsafe { ffi::close(fd) };
        }
        result
    }

    fn modeset(fd: i32) -> Result<Self, ()> {
        let mut crtc_id = 0u32;
        let mut connector_id = 0u32;
        let mut resources = DrmResources {
            crtc_ids: (&mut crtc_id as *mut u32) as u64,
            connector_ids: (&mut connector_id as *mut u32) as u64,
            crtc_count: 1,
            connector_count: 1,
            ..DrmResources::default()
        };
        // SAFETY: resources 引用的 crtc_id / connector_id 在 ioctl 期间有效。
        if unsafe {
            ffi::ioctl(
                fd,
                ffi::DRM_IOCTL_MODE_GETRESOURCES,
                (&mut resources as *mut DrmResources).cast(),
            )
        } < 0
        {
            return Err(());
        }
        if resources.crtc_count == 0 || resources.connector_count == 0 {
            return Err(());
        }
        let mut mode = DrmMode::default();
        let mut connector = DrmConnector {
            modes: (&mut mode as *mut DrmMode) as u64,
            mode_count: 1,
            connector_id,
            ..DrmConnector::default()
        };
        // SAFETY: connector.modes 指向有效的 DrmMode，容量为 1。
        if unsafe {
            ffi::ioctl(
                fd,
                ffi::DRM_IOCTL_MODE_GETCONNECTOR,
                (&mut connector as *mut DrmConnector).cast(),
            )
        } < 0
        {
            return Err(());
        }
        if connector.mode_count == 0 || mode.hdisplay == 0 || mode.vdisplay == 0 {
            return Err(());
        }
        let width = usize::from(mode.hdisplay);
        let height = usize::from(mode.vdisplay);
        let mut create = DrmDumbCreate {
            width: u32::from(mode.hdisplay),
            height: u32::from(mode.vdisplay),
            bpp: 32,
            ..DrmDumbCreate::default()
        };
        // SAFETY: create 在 ioctl 期间有效，内核回写 handle/pitch/size。
        if unsafe {
            ffi::ioctl(
                fd,
                ffi::DRM_IOCTL_MODE_CREATE_DUMB,
                (&mut create as *mut DrmDumbCreate).cast(),
            )
        } < 0
        {
            return Err(());
        }
        let mut scanout = Self::map_and_bind(fd, &mode, create, width, height)?;
        // SAFETY: crtc.connectors 指向有效 connector id，mode 来自 GETCONNECTOR。
        let mut crtc = DrmCrtc {
            connectors: (&connector_id as *const u32) as u64,
            connector_count: 1,
            crtc_id,
            framebuffer_id: scanout.framebuffer_id,
            mode_valid: 1,
            mode,
            ..DrmCrtc::default()
        };
        loop {
            let result = unsafe {
                ffi::ioctl(
                    fd,
                    ffi::DRM_IOCTL_MODE_SETCRTC,
                    (&mut crtc as *mut DrmCrtc).cast(),
                )
            };
            if result >= 0 {
                break;
            }
            if ffi::errno() != ffi::EINTR {
                scanout.release_fb();
                return Err(());
            }
        }
        Ok(scanout)
    }

    fn map_and_bind(
        fd: i32,
        mode: &DrmMode,
        create: DrmDumbCreate,
        width: usize,
        height: usize,
    ) -> Result<Self, ()> {
        let size = usize::try_from(create.size).map_err(|_| ())?;
        let mut map = DrmDumbMap {
            handle: create.handle,
            ..DrmDumbMap::default()
        };
        // SAFETY: map 在 ioctl 期间有效。
        if unsafe {
            ffi::ioctl(
                fd,
                ffi::DRM_IOCTL_MODE_MAP_DUMB,
                (&mut map as *mut DrmDumbMap).cast(),
            )
        } < 0
        {
            destroy_dumb(fd, create.handle);
            return Err(());
        }
        // SAFETY: offset 来自 MAP_DUMB，length 为 CREATE_DUMB 报告的 size。
        let pixels = unsafe {
            ffi::mmap(
                core::ptr::null_mut(),
                size,
                ffi::PROT_READ | ffi::PROT_WRITE,
                ffi::MAP_SHARED,
                fd,
                map.offset as i64,
            )
        };
        if pixels as usize == usize::MAX {
            destroy_dumb(fd, create.handle);
            return Err(());
        }
        let mut framebuffer = DrmFramebuffer {
            width: u32::from(mode.hdisplay),
            height: u32::from(mode.vdisplay),
            pitch: create.pitch,
            bpp: 32,
            depth: 24,
            handle: create.handle,
            ..DrmFramebuffer::default()
        };
        // SAFETY: framebuffer 在 ioctl 期间有效。
        if unsafe {
            ffi::ioctl(
                fd,
                ffi::DRM_IOCTL_MODE_ADDFB,
                (&mut framebuffer as *mut DrmFramebuffer).cast(),
            )
        } < 0
        {
            // SAFETY: pixels/size 是上面成功的映射。
            unsafe { ffi::munmap(pixels, size) };
            destroy_dumb(fd, create.handle);
            return Err(());
        }
        Ok(Self {
            fd,
            framebuffer_id: framebuffer.framebuffer_id,
            handle: create.handle,
            pixels: pixels.cast(),
            size,
            pitch: create.pitch as usize,
            mode: Mode { width, height },
        })
    }

    /// DRM fd：握手时经 `SCM_RIGHTS` dup 给客户端，也用于 `MAP_DUMB`
    /// 客户端 handle（共享 OFD / handle namespace）。
    pub fn drm_fd(&self) -> i32 {
        self.fd
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 当前 scanout fb 的可变像素视图；仅在事件循环的合成阶段使用。
    pub fn frame(&mut self) -> Frame {
        Frame {
            pixels: self.pixels,
            pitch: self.pitch,
            width: self.mode.width,
            height: self.mode.height,
        }
    }

    /// 把 damage clip 经 `DIRTYFB` 提交给内核；同步阻塞，`EINTR` 时重试。
    ///
    /// clip 先裁剪到屏幕内，超过 32 个时坍缩为单个 union clip。
    pub fn present(&self, damage: &[Rect]) {
        let screen = Rect::new(0, 0, self.mode.width as i32, self.mode.height as i32);
        let mut clips = [DrmClip::default(); MAX_CLIPS];
        let mut clip_count = 0usize;
        let mut union = None::<Rect>;
        for rect in damage {
            let clipped = rect.intersect(screen);
            if clipped.is_empty() {
                continue;
            }
            union = Some(match union {
                None => clipped,
                Some(previous) => previous.union(clipped),
            });
            if clip_count < MAX_CLIPS {
                clips[clip_count] = clip(clipped);
                clip_count += 1;
            }
        }
        let Some(union) = union else {
            return;
        };
        if clip_count == MAX_CLIPS && damage.len() > MAX_CLIPS {
            clips[0] = clip(union);
            clip_count = 1;
        }
        let mut dirty = DrmDirty {
            framebuffer_id: self.framebuffer_id,
            flags: 0,
            color: 0,
            clip_count: clip_count as u32,
            clips: clips.as_ptr() as u64,
        };
        loop {
            // SAFETY: dirty.clips 指向本栈帧内有效的 clip 数组。
            let result = unsafe {
                ffi::ioctl(
                    self.fd,
                    ffi::DRM_IOCTL_MODE_DIRTYFB,
                    (&mut dirty as *mut DrmDirty).cast(),
                )
            };
            if result >= 0 || ffi::errno() != ffi::EINTR {
                return;
            }
        }
    }

    /// 释放 fb 与 GEM handle（open 失败路径 / Drop 用）。
    fn release_fb(&mut self) {
        // SAFETY: pixels/size 为本对象持有的映射，handle 归本对象所有。
        unsafe { ffi::munmap(self.pixels.cast::<c_void>(), self.size) };
        destroy_dumb(self.fd, self.handle);
    }
}

impl Drop for Scanout {
    fn drop(&mut self) {
        self.release_fb();
        // SAFETY: fd 为本对象持有的有效描述符。
        unsafe { ffi::close(self.fd) };
    }
}

/// 销毁桌面持有的 GEM handle。客户端 surface 的 handle 所有权在
/// `CREATE_SURFACE` 提及时已转移给桌面，客户端绝不调用 DESTROY_DUMB。
pub fn destroy_dumb(fd: i32, handle: u32) {
    let mut handle = handle;
    // SAFETY: handle 是调用方持有的有效 dumb buffer handle。
    unsafe {
        ffi::ioctl(
            fd,
            ffi::DRM_IOCTL_MODE_DESTROY_DUMB,
            (&mut handle as *mut u32).cast(),
        )
    };
}

/// `MAP_DUMB` + `mmap` 一个（客户端创建、所有权已转移给桌面的）GEM handle，
/// 返回映射指针；`size` 由调用方按 `width * 4 * height` 计算（内核 dumb
/// pitch 恒为 `width * 4`）。失败时 handle 仍归调用方负责销毁。
pub fn map_dumb_buffer(fd: i32, handle: u32, size: usize) -> Result<*mut u32, ()> {
    let mut map = DrmDumbMap {
        handle,
        ..DrmDumbMap::default()
    };
    // SAFETY: map 在 ioctl 期间有效。
    if unsafe {
        ffi::ioctl(
            fd,
            ffi::DRM_IOCTL_MODE_MAP_DUMB,
            (&mut map as *mut DrmDumbMap).cast(),
        )
    } < 0
    {
        return Err(());
    }
    // SAFETY: offset 来自 MAP_DUMB，length 为该 dumb buffer 的实际大小。
    let pixels = unsafe {
        ffi::mmap(
            core::ptr::null_mut(),
            size,
            ffi::PROT_READ | ffi::PROT_WRITE,
            ffi::MAP_SHARED,
            fd,
            map.offset as i64,
        )
    };
    if pixels as usize == usize::MAX {
        return Err(());
    }
    Ok(pixels.cast())
}

fn clip(rect: Rect) -> DrmClip {
    DrmClip {
        x1: rect.x1 as u16,
        y1: rect.y1 as u16,
        x2: rect.x2 as u16,
        y2: rect.y2 as u16,
    }
}
