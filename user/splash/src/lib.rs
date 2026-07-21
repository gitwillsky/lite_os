#![no_std]
#![no_main]

//! LiteOS 启动画面：sysinit 阶段第一个打开 `/dev/dri/card0` 的进程。
//!
//! # 启动时序
//!
//! 1. `main` 作为 init 的 sysinit 进程运行：打开 card0（首个 open 自动成为 DRM
//!    master）、modeset、绘制启动画面，然后 `DROP_MASTER` 释放 master——desktop
//!    之后必须能重新成为 master 才能 modeset，缺少本调用其 SETCRTC 将永远失败。
//! 2. `fork` 后父进程立即 `_exit(0)`，init 的 sysinit 阶段得以继续拉起 desktop
//!    等 respawn 项；子进程继承同一 OFD，父进程退出不会触发内核回收 scanout/fb，
//!    动画依靠不需要 master 的 DIRTYFB 继续提交。
//! 3. 任一步失败（无 GPU 的 nographic 启动最常见）都静默 `_exit(0)`：splash 只是
//!    装饰，绝不打印、绝不读 stdin，runtime gate 的 UART 通道不能被干扰。
//!
//! # Safety model
//!
//! 1. DRM fd、GEM 映射与 framebuffer 由本进程独占；`Canvas` 的几何全部来自内核
//!    `CREATE_DUMB` 返回值并在构造前校验，每行切片访问都不越出 `pitch`。
//! 2. 所有 FFI 缓冲在整个（同步阻塞的）syscall 期间保持有效，结构体布局与 Linux
//!    UAPI 一致并有编译期大小断言。
//! 3. 子进程不安装任何信号 handler：SIGTERM 默认 disposition 直接终止，内核随后
//!    自动回收 OFD 上的 scanout/fb，无需显式 RMFB。

mod ffi;
mod render;

use core::{ffi::c_int, panic::PanicInfo, ptr};

use ffi::{
    DrmConnector, DrmCrtc, DrmDirty, DrmDumbCreate, DrmDumbMap, DrmFramebuffer, DrmMode,
    DrmResources,
};
use render::Canvas;

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    run();
    // 成功路径由子进程接管动画、父进程到达此处；失败路径同样到达此处。
    // 一律以 0 退出，sysinit 不因装饰程序缺失而阻断启动。
    // SAFETY: 进程使命结束，直接终止。
    unsafe { ffi::_exit(0) }
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    // 保持静默：panic 信息不得污染 UART。
    // SAFETY: 进程已不可恢复，直接终止。
    unsafe { ffi::_exit(125) }
}

/// 执行完整启动流程；任何失败返回 `None`，由 `main` 静默 `_exit(0)`。
fn run() -> Option<()> {
    // SAFETY: 路径为静态 NUL 结尾字符串；O_CLOEXEC 防止 fd 泄漏给后续 exec。
    let fd = unsafe {
        ffi::open(
            ffi::c_str(b"/dev/dri/card0\0"),
            ffi::O_RDWR | ffi::O_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return None;
    }
    let (crtc_id, connector_id) = query_ids(fd)?;
    let mode = query_mode(fd, connector_id)?;
    let (framebuffer_id, mut canvas) = create_buffer(fd, mode)?;
    canvas.fill(0);
    // bootlogo 运行时从 rootfs 读入（资产随镜像分发，不内嵌进二进制）；
    // 缺失或损坏时静默跳过（保留黑屏），与 splash 的装饰定位一致。
    if let Some((logo_pointer, logo_size)) = ffi::read_file(b"/usr/share/liteos/bootlogo.xrgb\0") {
        // SAFETY: logo_pointer/logo_size 来自 read_file 的匿名映射，munmap 前有效。
        let logo = unsafe { core::slice::from_raw_parts(logo_pointer as *const u8, logo_size) };
        canvas.draw_bootlogo(logo);
        // SAFETY: 映射由本函数持有，draw_bootlogo 返回后不再访问。
        unsafe { ffi::munmap(logo_pointer, logo_size) };
    }
    let track = canvas.track_origin();
    canvas.draw_track(track.0, track.1);
    set_crtc(fd, crtc_id, connector_id, framebuffer_id, mode)?;
    // SETCRTC 只切换扫描源，软件渲染写入的像素需要一次整幅 DIRTYFB 保证落屏。
    dirty(fd, framebuffer_id, None);
    // 释放 master 必须在 fork 之前：父子共享同一 OFD，只需调用一次。
    if unsafe { ffi::drm_ioctl(fd, ffi::DRM_IOCTL_DROP_MASTER, ptr::null_mut()) } < 0 {
        return None;
    }
    // SAFETY: fork 无参数。
    match unsafe { ffi::fork() } {
        // 父进程：sysinit 完成，返回后由 main `_exit(0)`。
        child if child > 0 => None,
        0 => {
            write_pid_file();
            animate(fd, framebuffer_id, &mut canvas, track);
        }
        _ => None,
    }
}

/// 进度条动画循环：每 100ms 前进一步，只向内核提交轨道矩形。
///
/// 收到 SIGTERM 时按默认 disposition 直接死亡，不做任何处理。
fn animate(fd: c_int, framebuffer_id: u32, canvas: &mut Canvas, track: (usize, usize)) -> ! {
    let mut offset = 0usize;
    loop {
        canvas.draw_sliders(track.0, track.1, offset);
        dirty(fd, framebuffer_id, Some(track));
        // poll 作为 100ms 定时器（console-session 先例）；EINTR/错误直接继续下一帧。
        // SAFETY: 空描述符集纯作定时，无缓冲区要求。
        unsafe { ffi::poll(ptr::null_mut(), 0, 100) };
        offset += render::SLIDER_STEP;
        if offset > render::max_slider_offset() {
            offset = 0;
        }
    }
}

/// 提交脏矩形；`region` 为 `None` 时整幅提交。失败静默忽略，下一帧会重试。
fn dirty(fd: c_int, framebuffer_id: u32, region: Option<(usize, usize)>) {
    let clip = region.and_then(|(x, y)| {
        Some(ffi::DrmClip {
            x1: u16::try_from(x).ok()?,
            y1: u16::try_from(y).ok()?,
            // 半开矩形：右/下边界取轨道外沿。
            x2: u16::try_from(x + render::TRACK_WIDTH).ok()?,
            y2: u16::try_from(y + render::TRACK_HEIGHT).ok()?,
        })
    });
    let mut request = DrmDirty {
        framebuffer_id,
        flags: 0,
        color: 0,
        // 坐标超出 u16（不可能在正常屏幕上发生）时退化为整幅提交。
        clip_count: u32::from(clip.is_some()),
        clips: clip
            .as_ref()
            .map_or(0, |clip| (clip as *const ffi::DrmClip) as u64),
    };
    // SAFETY: request 与其指向的 clip 在同步阻塞的 DIRTYFB 期间保持有效。
    unsafe { ffi::drm_ioctl(fd, ffi::DRM_IOCTL_MODE_DIRTYFB, (&mut request as *mut DrmDirty).cast()) };
}

/// GETRESOURCES 取固定拓扑下的 CRTC 与 connector ID（各取第一个）。
fn query_ids(fd: c_int) -> Option<(u32, u32)> {
    let mut crtc_id = 0u32;
    let mut connector_id = 0u32;
    let mut resources = DrmResources {
        crtc_ids: (&mut crtc_id as *mut u32) as u64,
        connector_ids: (&mut connector_id as *mut u32) as u64,
        crtc_count: 1,
        connector_count: 1,
        ..DrmResources::default()
    };
    // SAFETY: resources 指向的两个 u32 在 ioctl 期间有效；内核按 count 上限写入。
    if unsafe {
        ffi::drm_ioctl(
            fd,
            ffi::DRM_IOCTL_MODE_GETRESOURCES,
            (&mut resources as *mut DrmResources).cast(),
        )
    } < 0
    {
        return None;
    }
    if resources.crtc_count == 0 || resources.connector_count == 0 {
        return None;
    }
    Some((crtc_id, connector_id))
}

/// GETCONNECTOR 取 preferred mode（内核返回的 mode 列表首项）。
fn query_mode(fd: c_int, connector_id: u32) -> Option<DrmMode> {
    let mut mode = DrmMode::default();
    let mut connector = DrmConnector {
        modes: (&mut mode as *mut DrmMode) as u64,
        mode_count: 1,
        connector_id,
        ..DrmConnector::default()
    };
    // SAFETY: connector 指向的 mode 在 ioctl 期间有效；内核按 mode_count 上限写入。
    if unsafe {
        ffi::drm_ioctl(
            fd,
            ffi::DRM_IOCTL_MODE_GETCONNECTOR,
            (&mut connector as *mut DrmConnector).cast(),
        )
    } < 0
    {
        return None;
    }
    if connector.mode_count == 0 || mode.hdisplay == 0 || mode.vdisplay == 0 {
        return None;
    }
    Some(mode)
}

/// CREATE_DUMB + MAP_DUMB + mmap + ADDFB，返回（framebuffer ID, 帧缓冲视图）。
///
/// 中途失败不显式销毁 handle：调用方随即 `_exit`，内核回收 OFD 时统一清理。
fn create_buffer(fd: c_int, mode: DrmMode) -> Option<(u32, Canvas)> {
    let width = usize::from(mode.hdisplay);
    let height = usize::from(mode.vdisplay);
    let mut create = DrmDumbCreate {
        width: u32::from(mode.hdisplay),
        height: u32::from(mode.vdisplay),
        bpp: 32,
        ..DrmDumbCreate::default()
    };
    // SAFETY: create 在 ioctl 期间有效；成功后内核填回 handle/pitch/size。
    if unsafe {
        ffi::drm_ioctl(
            fd,
            ffi::DRM_IOCTL_MODE_CREATE_DUMB,
            (&mut create as *mut DrmDumbCreate).cast(),
        )
    } < 0
    {
        return None;
    }
    let size = usize::try_from(create.size).ok()?;
    let pitch = usize::try_from(create.pitch).ok()?;
    // 行宽与总大小校验是 Canvas 每行切片访问不越界的前提。
    if pitch < width.checked_mul(4)? || pitch.checked_mul(height)? > size {
        return None;
    }
    let mut map = DrmDumbMap {
        handle: create.handle,
        ..DrmDumbMap::default()
    };
    // SAFETY: map 在 ioctl 期间有效；成功后内核填回 mmap 偏移。
    if unsafe { ffi::drm_ioctl(fd, ffi::DRM_IOCTL_MODE_MAP_DUMB, (&mut map as *mut DrmDumbMap).cast()) } < 0 {
        return None;
    }
    // SAFETY: 长度与偏移均来自内核；MAP_SHARED 使写入对其他映射方可见。
    let pixels = unsafe {
        ffi::mmap(
            ptr::null_mut(),
            size,
            ffi::PROT_READ | ffi::PROT_WRITE,
            ffi::MAP_SHARED,
            fd,
            map.offset as i64,
        )
    };
    if pixels as usize == usize::MAX {
        return None;
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
    // SAFETY: framebuffer 在 ioctl 期间有效；成功后内核填回 framebuffer_id。
    if unsafe { ffi::drm_ioctl(fd, ffi::DRM_IOCTL_MODE_ADDFB, (&mut framebuffer as *mut DrmFramebuffer).cast()) } < 0 {
        return None;
    }
    // SAFETY: 映射长度已按 pitch*height <= size 校验；进程永不 munmap，
    // 映射贯穿 Canvas 的整个存活期。
    let canvas = unsafe { Canvas::new(pixels.cast(), pitch, width, height) };
    Some((framebuffer.framebuffer_id, canvas))
}

/// SETCRTC（同步阻塞）：fb 铺满全屏，绑定唯一 connector。
fn set_crtc(
    fd: c_int,
    crtc_id: u32,
    connector_id: u32,
    framebuffer_id: u32,
    mode: DrmMode,
) -> Option<()> {
    let mut crtc = DrmCrtc {
        connectors: (&connector_id as *const u32) as u64,
        connector_count: 1,
        crtc_id,
        framebuffer_id,
        x: 0,
        y: 0,
        mode_valid: 1,
        mode,
        ..DrmCrtc::default()
    };
    // SAFETY: crtc 与其指向的 connector_id 在同步阻塞的 SETCRTC 期间保持有效。
    if unsafe { ffi::drm_ioctl(fd, ffi::DRM_IOCTL_MODE_SETCRTC, (&mut crtc as *mut DrmCrtc).cast()) } < 0 {
        return None;
    }
    Some(())
}

/// 子进程写 `/run/splash.pid`（十进制 pid + 换行），供 desktop 就绪后按 pid 终止动画。
/// 文件系统不可用等失败静默忽略，不影响动画本身。
fn write_pid_file() {
    // SAFETY: 路径为静态 NUL 结尾字符串；O_CREAT 需要有效的 mode 参数。
    let fd = unsafe {
        ffi::open(
            ffi::c_str(b"/run/splash.pid\0"),
            ffi::O_WRONLY | ffi::O_CREAT | ffi::O_TRUNC | ffi::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        return;
    }
    // SAFETY: getpid 无参数。
    let pid = unsafe { ffi::getpid() };
    let mut buffer = [0u8; 12];
    let mut cursor = buffer.len() - 1;
    buffer[cursor] = b'\n';
    let mut value = pid.max(1) as u32;
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    // SAFETY: buffer 在 write 期间有效。
    unsafe { ffi::write(fd, buffer[cursor..].as_ptr().cast(), buffer.len() - cursor) };
    // SAFETY: fd 为本进程持有。
    unsafe { ffi::close(fd) };
}
