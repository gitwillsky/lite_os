//! 唯一的 DRM master：选中的 CPU/GPU 后端独占一对扫描目标与 page flip。

use std::io;

use linux_uapi::drm::{DrmDevice, DumbBuffer, Topology};

struct Target {
    buffer: DumbBuffer,
    framebuffer: u32,
    device: DrmDevice,
}
impl Drop for Target {
    fn drop(&mut self) {
        // 构造失败与正常退出都先移除 KMS 引用，再由字段析构释放 GEM。
        let _ = self.device.remove_framebuffer(self.framebuffer);
    }
}
enum Backend {
    Cpu([Target; 2]),
    Gpu(crate::compose::gpu::Gpu),
}
impl Backend {
    fn framebuffer(&self, back: usize) -> u32 {
        match self {
            Self::Cpu(targets) => targets[back].framebuffer,
            Self::Gpu(gpu) => gpu.framebuffer(back),
        }
    }
}

/// 单一 connector/CRTC 的显示输出。
///
/// 画面总是先画进 `back`；`show_first` 把它交给 CRTC 并交换前后缓冲。
pub struct Scanout {
    device: DrmDevice,
    topology: Topology,
    backend: Backend,
    back: usize,
    /// 在途 flip 的序号；同一时刻至多一个 flip，缺少这条约束会让第二次 `PAGE_FLIP` 被内核以 `EBUSY` 拒绝。
    pending: Option<u64>,
    sequence: u64,
}

impl Scanout {
    /// 打开 `path`，成为 DRM master，按 connector 的首选模式建立双缓冲并分配 framebuffer。
    ///
    /// # Errors
    ///
    /// 设备不存在、已有另一个 master、没有可用 connector，或 dumb buffer/framebuffer 创建失败。
    pub fn open(path: &str) -> io::Result<Self> {
        let device = DrmDevice::open(path)?;
        device.set_master()?;
        let topology = device.query_topology()?;
        let (width, height) = (
            u32::from(topology.mode.width()),
            u32::from(topology.mode.height()),
        );
        // 1. 先协商能力，再分配唯一后端。Unsupported 之外的错误必须向上传播。
        // 2. 启动画面也消费所选后端的像素存储，不能因此永久保留另一对全屏目标。
        let backend = match device.validate_virgl2() {
            Ok(()) => Backend::Gpu(crate::compose::gpu::Gpu::new(&device, width, height)?),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                let target = || -> io::Result<Target> {
                    let buffer = device.create_dumb(width, height)?;
                    let framebuffer = device.add_framebuffer(&buffer, 24)?;
                    Ok(Target {
                        buffer,
                        framebuffer,
                        device: device.clone(),
                    })
                };
                Backend::Cpu([target()?, target()?])
            }
            Err(error) => return Err(error),
        };
        println!(
            "compositor: composition backend={}",
            match &backend {
                Backend::Cpu(_) => "CPU",
                Backend::Gpu(_) => "VirGL2",
            }
        );
        Ok(Self {
            device,
            topology,
            backend,
            back: 0,
            pending: None,
            sequence: 0,
        })
    }

    pub fn width(&self) -> usize {
        usize::from(self.topology.mode.width())
    }

    pub fn height(&self) -> usize {
        usize::from(self.topology.mode.height())
    }

    /// 在可写后缓冲绘制启动画面；flip 在途返回 WouldBlock，画面或设备错误直接传播。
    pub fn paint_boot(&mut self, scene: &[u8], permille: u16) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        match &mut self.backend {
            Backend::Cpu(targets) => {
                crate::boot::paint(&mut targets[self.back].buffer, scene, permille)
                    .map_err(io::Error::other)
            }
            Backend::Gpu(gpu) => gpu.paint_boot(self.back, scene, permille),
        }
    }

    /// 首次把后缓冲设为 CRTC 的扫描对象（modeset），并交换前后缓冲。
    ///
    /// # Errors
    ///
    /// 内核拒绝 modeset。
    pub fn show_first(&mut self) -> io::Result<()> {
        self.device
            .set_crtc(&self.topology, self.backend.framebuffer(self.back))?;
        self.back ^= 1;
        Ok(())
    }

    /// 是否有 flip 尚未完成。
    pub fn flip_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// 异步把后缓冲交给 CRTC；完成由 DRM fd 变为可读通知，随后调用 [`Scanout::complete_flip`]。
    ///
    /// # Errors
    ///
    /// 已有 flip 在途，或内核拒绝。
    pub fn request_flip(&mut self) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(io::Error::other("a page flip is already pending"));
        }
        self.sequence += 1;
        self.device.page_flip(
            &self.topology,
            self.backend.framebuffer(self.back),
            self.sequence,
        )?;
        self.pending = Some(self.sequence);
        Ok(())
    }

    /// 读出在途 flip 的完成事件并交换前后缓冲。
    ///
    /// # Errors
    ///
    /// 没有在途 flip、事件读取失败或序号不符。
    pub fn complete_flip(&mut self) -> io::Result<()> {
        let expected = self
            .pending
            .ok_or_else(|| io::Error::other("no page flip is pending"))?;
        let event = self.device.read_flip_event()?;
        if event.user_data != expected {
            return Err(io::Error::other(
                "flip completion for an unexpected sequence",
            ));
        }
        self.pending = None;
        if let Backend::Gpu(gpu) = &mut self.backend {
            gpu.complete();
        }
        self.back ^= 1;
        Ok(())
    }

    /// 按设备能力选择一次后端；CPU/GPU 消费同一标准 Wayland 内容、几何与叠加顺序。
    /// 已有 flip 时拒绝覆盖 back；渲染/提交失败直接返回错误。
    pub fn compose(
        &mut self,
        theme: &classic_theme::Theme,
        windows: &[crate::compose::Drawable<'_>],
        inversions: &[canvas::Rect],
        has_overlay: bool,
        overlay: impl FnOnce(canvas::Surface<'_>),
    ) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        match &mut self.backend {
            Backend::Gpu(gpu) => {
                gpu.render(self.back, theme, windows, inversions, has_overlay, overlay)?
            }
            Backend::Cpu(targets) => {
                if !crate::compose::compose(
                    &mut targets[self.back].buffer,
                    theme,
                    windows,
                    inversions,
                    overlay,
                ) {
                    return Err(io::Error::other("invalid scanout drawing surface"));
                }
            }
        }
        Ok(())
    }

    pub fn device(&self) -> &DrmDevice {
        &self.device
    }
}

impl Scanout {
    /// 显示或更新硬件光标：`position` 是热点在屏幕上的位置。
    ///
    /// # Errors
    ///
    /// 缓冲不是 64 × 64，或光标队列失败。
    pub fn show_cursor(
        &self,
        image: &DumbBuffer,
        position: (i32, i32),
        hotspot: (u32, u32),
    ) -> io::Result<()> {
        self.device
            .update_cursor(&self.topology, Some(image), position, hotspot)
    }

    /// 隐藏硬件光标；之后 [`Scanout::show_cursor`] 再次显示。
    pub fn hide_cursor(&self, position: (i32, i32)) -> io::Result<()> {
        self.device
            .update_cursor(&self.topology, None, position, (0, 0))
    }

    /// 只移动硬件光标，不改变图像。
    pub fn move_cursor(&self, position: (i32, i32)) -> io::Result<()> {
        self.device.move_cursor(&self.topology, position)
    }
}
