//! 开机画面：把 `boot-scene` 位图画到 boot scanout，并随启动里程碑推进进度条。
//!
//! 画面从 GPU 初始化后的第一刻起就存在，早于 scheduler、文件系统与 DRM；compositor 随后接着画同一张图，
//! 因此缩放、居中与进度几何全部来自共用 crate `boot-scene`，这里不保留第二份。
//! 画面纯属装饰：任何绘制失败只记录警告并停止更新，绝不阻止启动。

use alloc::vec::Vec;

use boot_scene::{KERNEL_PROGRESS_PERMILLE, Layout, ParseError, Scene, WIDTH};

use crate::drivers::{BootFramebuffer, DisplayError, DisplayMode, DisplayRect};

/// 内核启动里程碑；每个里程碑对应进度条的一个固定千分比。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Milestone {
    /// 命名空间与 IPC/socket 全局状态就绪。
    Namespaces,
    /// 文件系统与 scheduler 就绪。
    Scheduler,
    /// 平台设备与块设备发布完成。
    Devices,
    /// 音频初始化完成；这是 DRM 接管前内核的最后一步，进度到达 [`KERNEL_PROGRESS_PERMILLE`]。
    Audio,
}

impl Milestone {
    const fn permille(self) -> u16 {
        match self {
            Self::Namespaces => 100,
            Self::Scheduler => 200,
            Self::Devices => 300,
            Self::Audio => KERNEL_PROGRESS_PERMILLE,
        }
    }
}

/// 绘制失败的原因，只用于日志。
enum Failure {
    Scene(ParseError),
    ScreenTooSmall,
    OutOfMemory,
    Display(DisplayError),
}

impl core::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scene(error) => write!(formatter, "invalid scene bitmap ({error:?})"),
            Self::ScreenTooSmall => formatter.write_str("display is smaller than 640x480"),
            Self::OutOfMemory => formatter.write_str("out of memory"),
            Self::Display(error) => write!(formatter, "display refused the update ({error:?})"),
        }
    }
}

impl From<DisplayError> for Failure {
    fn from(error: DisplayError) -> Self {
        Self::Display(error)
    }
}

struct Canvas<'a> {
    framebuffer: &'a dyn BootFramebuffer,
    scene: Scene<'static>,
    layout: Layout,
    mode: DisplayMode,
    /// 进度条已经涂成填充色的屏幕像素宽度；进度只增不减，下一次只重画新增部分。
    drawn: usize,
}

/// 开机画面状态；没有显示设备或绘制失败时是一个什么都不做的空壳，调用方不需要区分。
pub(crate) struct BootScreen<'a> {
    canvas: Option<Canvas<'a>>,
}

impl<'a> BootScreen<'a> {
    /// 把画面整幅画到 boot scanout 上，进度条为空。
    ///
    /// # Parameters
    ///
    /// - `framebuffer`: 启动期同步帧缓冲；`None` 表示没有显示设备。
    /// - `scene`: 完整的 `LBS1` 位图字节，由装配层按构建选择的素材提供。
    pub(crate) fn start(
        framebuffer: Option<&'a dyn BootFramebuffer>,
        scene: &'static [u8],
    ) -> Self {
        let canvas = framebuffer.and_then(|framebuffer| match Canvas::draw(framebuffer, scene) {
            Ok(canvas) => Some(canvas),
            Err(failure) => {
                warn!("boot screen disabled: {failure}");
                None
            }
        });
        Self { canvas }
    }

    /// 把进度条推进到 `milestone` 对应的位置；更新失败时关闭后续更新。
    pub(crate) fn advance(&mut self, milestone: Milestone) {
        let Some(canvas) = &mut self.canvas else {
            return;
        };
        if let Err(failure) = canvas.fill_to(milestone.permille()) {
            warn!("boot screen progress stopped: {failure}");
            self.canvas = None;
        }
    }
}

impl<'a> Canvas<'a> {
    fn draw(framebuffer: &'a dyn BootFramebuffer, bytes: &'static [u8]) -> Result<Self, Failure> {
        let scene = Scene::parse(bytes).map_err(Failure::Scene)?;
        let mode = framebuffer.mode();
        let layout = Layout::fit(mode.width as usize, mode.height as usize)
            .ok_or(Failure::ScreenTooSmall)?;
        let pitch = mode.pitch as usize;
        // 先整屏清成黑色：位图外的区域不能留下 allocator 返回的旧内容。
        let zeros = [0u8; 4096];
        let total = pitch * mode.height as usize;
        let mut offset = 0;
        while offset < total {
            let length = zeros.len().min(total - offset);
            framebuffer.write(offset, &zeros[..length])?;
            offset += length;
        }
        paint_scene(framebuffer, &scene, &layout, pitch)?;
        framebuffer.flush(DisplayRect { x: 0, y: 0, width: mode.width, height: mode.height })?;
        Ok(Self { framebuffer, scene, layout, mode, drawn: 0 })
    }

    fn fill_to(&mut self, permille: u16) -> Result<(), Failure> {
        let Some(rect) = self.layout.progress_rect(&self.scene, permille) else {
            return Ok(());
        };
        if rect.width <= self.drawn {
            return Ok(());
        }
        let fresh = rect.width - self.drawn;
        let mut line = Vec::new();
        line.try_reserve_exact(fresh * 4).map_err(|_| Failure::OutOfMemory)?;
        let pixel = self.scene.fill_color().to_le_bytes();
        for _ in 0..fresh {
            line.extend_from_slice(&pixel);
        }
        let x = rect.x + self.drawn;
        let pitch = self.mode.pitch as usize;
        for dy in 0..rect.height {
            self.framebuffer.write((rect.y + dy) * pitch + x * 4, &line)?;
        }
        self.framebuffer.flush(DisplayRect {
            x: x as u32,
            y: rect.y as u32,
            width: fresh as u32,
            height: rect.height as u32,
        })?;
        self.drawn = rect.width;
        Ok(())
    }
}

/// 逐行解码位图，按整数倍放大后居中写入帧缓冲（XRGB8888，字节序 B、G、R、X）。
fn paint_scene(
    framebuffer: &dyn BootFramebuffer,
    scene: &Scene<'_>,
    layout: &Layout,
    pitch: usize,
) -> Result<(), Failure> {
    let mut line = Vec::new();
    line.try_reserve_exact(WIDTH * layout.scale * 4).map_err(|_| Failure::OutOfMemory)?;
    let mut indices = [0u8; WIDTH];
    let mut rows = scene.rows();
    let mut row = 0;
    while rows.next_row(&mut indices) {
        line.clear();
        for &index in &indices {
            let pixel = scene.color(index).to_le_bytes();
            for _ in 0..layout.scale {
                line.extend_from_slice(&pixel);
            }
        }
        for repeat in 0..layout.scale {
            let y = layout.y + row * layout.scale + repeat;
            framebuffer.write(y * pitch + layout.x * 4, &line)?;
        }
        row += 1;
    }
    Ok(())
}
